/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use crate::jmap::JMAPTest;
use ahash::AHashSet;
use futures::{SinkExt, StreamExt};
use jmap_client::{
    DataType, PushObject,
    client_ws::WebSocketMessage,
    core::{
        response::{Response, TaggedMethodResponse},
        set::SetObject,
    },
};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};

pub async fn test(params: &mut JMAPTest) {
    println!("Running WebSockets tests...");

    // Authenticate all accounts
    let account = params.account("jdoe@example.com");
    let client = account.client();

    let mut ws_stream = client.connect_ws().await.unwrap();

    let (stream_tx, mut stream_rx) = mpsc::channel::<WebSocketMessage>(100);

    tokio::spawn(async move {
        while let Some(change) = ws_stream.next().await {
            stream_tx.send(change.unwrap()).await.unwrap();
        }
    });

    // Create mailbox
    let mut request = client.build();
    let create_id = request
        .set_mailbox()
        .create()
        .name("WebSocket Test")
        .create_id()
        .unwrap();
    let request_id = request.send_ws().await.unwrap();
    let mut response = expect_response(&mut stream_rx).await;
    assert_eq!(request_id, response.request_id().unwrap());
    let mailbox_id = response
        .pop_method_response()
        .unwrap()
        .unwrap_set_mailbox()
        .unwrap()
        .created(&create_id)
        .unwrap()
        .take_id();

    // Enable push notifications
    client
        .enable_push_ws(None::<Vec<_>>, None::<&str>)
        .await
        .unwrap();

    // Make changes over standard HTTP and expect a push notification via WebSockets
    client
        .mailbox_update_sort_order(&mailbox_id, 1)
        .await
        .unwrap();
    assert_state(&mut stream_rx, account.id_string(), &[DataType::Mailbox]).await;

    // Multiple changes should be grouped and delivered in intervals
    for num in 0..5 {
        client
            .mailbox_update_sort_order(&mailbox_id, num)
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_state(&mut stream_rx, account.id_string(), &[DataType::Mailbox]).await;
    expect_nothing(&mut stream_rx).await;

    // Disable push notifications
    client.disable_push_ws().await.unwrap();

    // No more changes should be received
    let mut request = client.build();
    request.set_mailbox().destroy([&mailbox_id]);
    request.send_ws().await.unwrap();
    expect_response(&mut stream_rx)
        .await
        .pop_method_response()
        .unwrap()
        .unwrap_set_mailbox()
        .unwrap()
        .destroyed(&mailbox_id)
        .unwrap();
    expect_nothing(&mut stream_rx).await;

    params.destroy_all_mailboxes(account).await;
    params.assert_is_empty().await;
}

async fn expect_response(
    stream_rx: &mut mpsc::Receiver<WebSocketMessage>,
) -> Response<TaggedMethodResponse> {
    match tokio::time::timeout(Duration::from_millis(100), stream_rx.recv()).await {
        Ok(Some(message)) => match message {
            WebSocketMessage::Response(response) => response,
            _ => panic!("Expected response, got: {:?}", message),
        },
        result => {
            panic!("Timeout waiting for websocket: {:?}", result);
        }
    }
}

async fn assert_state(
    stream_rx: &mut mpsc::Receiver<WebSocketMessage>,
    id: &str,
    state: &[DataType],
) {
    match tokio::time::timeout(Duration::from_millis(700), stream_rx.recv()).await {
        Ok(Some(message)) => match message {
            WebSocketMessage::PushNotification(PushObject::StateChange { changed }) => {
                assert_eq!(
                    changed
                        .get(id)
                        .unwrap()
                        .keys()
                        .collect::<AHashSet<&DataType>>(),
                    state.iter().collect::<AHashSet<&DataType>>()
                );
            }
            _ => panic!("Expected state change, got: {:?}", message),
        },
        result => {
            panic!("Timeout waiting for websocket: {:?}", result);
        }
    }
}

async fn expect_nothing(stream_rx: &mut mpsc::Receiver<WebSocketMessage>) {
    match tokio::time::timeout(Duration::from_millis(1000), stream_rx.recv()).await {
        Err(_) => {}
        message => {
            panic!("Received a message when expecting nothing: {:?}", message);
        }
    }
}

// WebSocket Ticket Authentication Tests
pub async fn test_ticket_auth(params: &mut JMAPTest) {
    println!("Running WebSocket Ticket Authentication tests...");

    let account = params.account("jdoe@example.com");
    let client = account.client();

    // Get an access token for the account
    let access_token = client.access_token();

    // Step 1: Request a WebSocket ticket
    println!("  - Testing ticket generation...");
    let ticket_response = reqwest::Client::builder()
        .timeout(Duration::from_millis(5000))
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap()
        .post("https://127.0.0.1:8899/jmap/ws/ticket")
        .bearer_auth(access_token)
        .send()
        .await
        .unwrap();

    assert_eq!(
        ticket_response.status().as_u16(),
        200,
        "Ticket request should succeed"
    );

    let ticket_json: serde_json::Value = ticket_response.json().await.unwrap();
    let ticket = ticket_json["value"].as_str().unwrap();
    assert!(!ticket.is_empty(), "Ticket should not be empty");
    println!("    Ticket generated successfully");

    // Step 2: Connect to WebSocket using the ticket
    println!("  - Testing WebSocket connection with ticket...");
    let ws_url = format!("wss://127.0.0.1:8899/jmap/ws?ticket={}", ticket);

    // Create a TLS connector that accepts invalid certs for testing
    let connector = native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();
    let connector = tokio_tungstenite::Connector::NativeTls(connector);

    let (mut ws_stream, _) = tokio_tungstenite::connect_async_tls_with_config(
        &ws_url,
        None,
        false,
        Some(connector),
    )
    .await
    .expect("WebSocket connection with valid ticket should succeed");

    println!("    WebSocket connected successfully with ticket");

    // Step 3: Send a JMAP request over WebSocket to verify it works
    println!("  - Testing JMAP request over ticket-authenticated WebSocket...");
    let jmap_request = serde_json::json!({
        "@type": "Request",
        "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
        "methodCalls": [
            ["Mailbox/get", {"accountId": account.id_string(), "ids": null}, "0"]
        ]
    });

    ws_stream
        .send(Message::Text(jmap_request.to_string().into()))
        .await
        .expect("Should be able to send JMAP request");

    // Wait for response
    let response = tokio::time::timeout(Duration::from_secs(5), ws_stream.next())
        .await
        .expect("Should receive response within timeout")
        .expect("Stream should not be closed")
        .expect("Message should be valid");

    match response {
        Message::Text(text) => {
            let json: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert!(
                json.get("methodResponses").is_some(),
                "Response should contain methodResponses"
            );
            println!("    JMAP request/response successful");
        }
        _ => panic!("Expected text message, got: {:?}", response),
    }

    // Close the WebSocket
    ws_stream.close(None).await.ok();

    // Step 4: Test that invalid ticket is rejected
    println!("  - Testing invalid ticket rejection...");
    let invalid_ws_url = "wss://127.0.0.1:8899/jmap/ws?ticket=invalid_ticket_value";

    let connector = native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();
    let connector = tokio_tungstenite::Connector::NativeTls(connector);

    let result = tokio_tungstenite::connect_async_tls_with_config(
        invalid_ws_url,
        None,
        false,
        Some(connector),
    )
    .await;

    assert!(
        result.is_err(),
        "WebSocket connection with invalid ticket should fail"
    );
    println!("    Invalid ticket correctly rejected");

    // Step 5: Test that ticket cannot be reused (optional, depends on implementation)
    // Tickets are single-use in some implementations

    // Step 6: Test ticket expiration (would need to wait for expiry time)
    // Skipped in automated tests to avoid long wait times

    println!("WebSocket Ticket Authentication tests completed successfully!");
}
