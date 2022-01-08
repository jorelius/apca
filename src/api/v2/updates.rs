// Copyright (C) 2019-2021 The apca Developers
// SPDX-License-Identifier: GPL-3.0-or-later

use std::borrow::Cow;

use async_trait::async_trait;

use futures::stream::Fuse;
use futures::stream::Map;
use futures::stream::SplitSink;
use futures::stream::SplitStream;
use futures::FutureExt as _;
use futures::Sink;
use futures::StreamExt as _;

use serde::Deserialize;
use serde::Serialize;
use serde_json::from_slice as json_from_slice;
use serde_json::from_str as json_from_str;
use serde_json::to_string as to_json;
use serde_json::Error as JsonError;

use tokio::net::TcpStream;

use tungstenite::MaybeTlsStream;
use tungstenite::WebSocketStream;

use websocket_util::subscribe;
use websocket_util::subscribe::MessageStream;
use websocket_util::tungstenite::Error as WebSocketError;
use websocket_util::wrap;
use websocket_util::wrap::Wrapper;

use crate::api::v2::order;
use crate::api_info::ApiInfo;
use crate::events::connect;
use crate::events::EventStream;
use crate::events::StreamType;
use crate::subscribable::Subscribable;
use crate::Error;


/// The status of a trade, as reported as part of a `TradeUpdate`.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
pub enum TradeStatus {
  /// The order has been received by Alpaca, and routed to exchanges for
  /// execution.
  #[serde(rename = "new")]
  New,
  /// The order has changed.
  #[serde(rename = "replaced")]
  Replaced,
  /// The order replacement has been rejected.
  #[serde(rename = "order_replace_rejected")]
  ReplaceRejected,
  /// The order has been partially filled.
  #[serde(rename = "partial_fill")]
  PartialFill,
  /// The order has been filled, and no further updates will occur for
  /// the order.
  #[serde(rename = "fill")]
  Filled,
  /// The order is done executing for the day, and will not receive
  /// further updates until the next trading day.
  #[serde(rename = "done_for_day")]
  DoneForDay,
  /// The order has been canceled, and no further updates will occur for
  /// the order.
  #[serde(rename = "canceled")]
  Canceled,
  /// The order cancellation has been rejected.
  #[serde(rename = "order_cancel_rejected")]
  CancelRejected,
  /// The order has expired, and no further updates will occur.
  #[serde(rename = "expired")]
  Expired,
  /// The order is waiting to be canceled.
  #[serde(rename = "pending_cancel")]
  PendingCancel,
  /// The order has been stopped, and a trade is guaranteed for the
  /// order, usually at a stated price or better, but has not yet
  /// occurred.
  #[serde(rename = "stopped")]
  Stopped,
  /// The order has been rejected, and no further updates will occur for
  /// the order.
  #[serde(rename = "rejected")]
  Rejected,
  /// The order has been suspended, and is not eligible for trading.
  /// This state only occurs on rare occasions.
  #[serde(rename = "suspended")]
  Suspended,
  /// The order has been received by Alpaca, and routed to the
  /// exchanges, but has not yet been accepted for execution.
  #[serde(rename = "pending_new")]
  PendingNew,
  /// The order is awaiting replacement.
  #[serde(rename = "pending_replace")]
  PendingReplace,
  /// The order has been completed for the day (either filled or done
  /// for day), but remaining settlement calculations are still pending.
  #[serde(rename = "calculated")]
  Calculated,
  /// Any other status that we have not accounted for.
  ///
  /// Note that having any such status should be considered a bug.
  #[serde(other, rename(serialize = "unknown"))]
  Unknown,
}


/// A type capturing the stream types we may subscribe to.
#[derive(Debug, Deserialize, Serialize)]
#[doc(hidden)]
pub struct Streams<'d> {
  /// A list of stream types.
  streams: Cow<'d, [StreamType]>,
}

impl<'d> From<&'d [StreamType]> for Streams<'d> {
  #[inline]
  fn from(src: &'d [StreamType]) -> Self {
    Self {
      streams: Cow::from(src),
    }
  }
}


/// The status reported in authentication control messages.
#[derive(Debug, Deserialize, PartialEq)]
enum AuthenticationStatus {
  /// The client has been authorized.
  #[serde(rename = "authorized")]
  Authorized,
  /// The client has not been authorized.
  #[serde(rename = "unauthorized")]
  Unauthorized,
}


/// The authentication related data provided in authentication control
/// messages.
#[derive(Debug, Deserialize)]
#[doc(hidden)]
pub struct Authentication {
  /// The status of an operation.
  #[serde(rename = "status")]
  status: AuthenticationStatus,
  /*
   * TODO: Right now we just ignore the `action` field, as we would
   *       not react on it anyway.
   */
}


/// A custom [`Result`]-style type that we can implement a foreign trait
/// on.
#[derive(Debug)]
#[doc(hidden)]
pub enum MessageResult<T, E> {
  /// The success value.
  Ok(T),
  /// The error value.
  Err(E),
}

impl<T, E> From<Result<T, E>> for MessageResult<T, E> {
  #[inline]
  fn from(result: Result<T, E>) -> Self {
    match result {
      Ok(t) => Self::Ok(t),
      Err(e) => Self::Err(e),
    }
  }
}


/// A control message "request" sent over a websocket channel.
#[derive(Debug, Serialize)]
#[serde(tag = "action", content = "data")]
enum Request<'d> {
  /// A control message indicating whether or not we were authenticated
  /// successfully.
  #[serde(rename = "authenticate")]
  Authenticate {
    #[serde(rename = "key_id")]
    key_id: &'d str,
    #[serde(rename = "secret_key")]
    secret: &'d str,
  },
  /// A control message detailing the streams we are subscribed to.
  #[serde(rename = "listen")]
  Listen(Streams<'d>),
}


/// An enumeration of the supported control messages.
#[derive(Debug)]
#[doc(hidden)]
pub enum ControlMessage {
  /// A control message indicating whether or not we were authenticated.
  AuthenticationMessage(Authentication),
  /// A control message indicating which streams we are
  /// subscribed/listening to now.
  ListeningMessage(Streams<'static>),
}


/// An enum representing the different messages we may receive over our
/// websocket channel.
#[derive(Debug, Deserialize)]
#[doc(hidden)]
#[serde(tag = "stream", content = "data")]
#[allow(clippy::large_enum_variant)]
pub enum TradeMessage {
  /// A trade update.
  #[serde(rename = "trade_updates")]
  TradeUpdate(TradeUpdate),
  /// A control message indicating whether or not we were authenticated
  /// successfully.
  #[serde(rename = "authorization")]
  AuthenticationMessage(Authentication),
  /// A control message detailing the streams we are subscribed to.
  #[serde(rename = "listening")]
  ListeningMessage(Streams<'static>),
}


/// A representation of a trade update that we receive through the
/// "trade_updates" stream.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[non_exhaustive]
pub struct TradeUpdate {
  /// The event that occurred.
  #[serde(rename = "event")]
  pub event: TradeStatus,
  /// The order associated with the trade.
  #[serde(rename = "order")]
  pub order: order::Order,
}


/// A websocket message that we tried to parse.
type ParsedMessage = MessageResult<Result<TradeMessage, JsonError>, WebSocketError>;

impl subscribe::Message for ParsedMessage {
  type UserMessage = Result<Result<TradeUpdate, JsonError>, WebSocketError>;
  type ControlMessage = ControlMessage;

  fn classify(self) -> subscribe::Classification<Self::UserMessage, Self::ControlMessage> {
    match self {
      MessageResult::Ok(Ok(message)) => match message {
        TradeMessage::TradeUpdate(update) => subscribe::Classification::UserMessage(Ok(Ok(update))),
        TradeMessage::AuthenticationMessage(authentication) => {
          subscribe::Classification::ControlMessage(ControlMessage::AuthenticationMessage(
            authentication,
          ))
        },
        TradeMessage::ListeningMessage(streams) => {
          subscribe::Classification::ControlMessage(ControlMessage::ListeningMessage(streams))
        },
      },
      // JSON errors are directly passed through.
      MessageResult::Ok(Err(err)) => subscribe::Classification::UserMessage(Ok(Err(err))),
      // WebSocket errors are also directly pushed through.
      MessageResult::Err(err) => subscribe::Classification::UserMessage(Err(err)),
    }
  }

  #[inline]
  fn is_error(user_message: &Self::UserMessage) -> bool {
    // Both outer `WebSocketError` and inner `JsonError` errors
    // constitute errors in our sense.
    user_message
      .as_ref()
      .map(|result| result.is_err())
      .unwrap_or(true)
  }
}


/// A subscription allowing certain control operations pertaining trade
/// update retrieval.
#[derive(Debug)]
pub struct Subscription<S>(subscribe::Subscription<S, ParsedMessage, wrap::Message>);

impl<S> Subscription<S>
where
  S: Sink<wrap::Message> + Unpin,
{
  /// Authenticate the connection using Alpaca credentials.
  async fn authenticate(
    &mut self,
    key_id: &str,
    secret: &str,
  ) -> Result<Result<(), Error>, S::Error> {
    let request = Request::Authenticate { key_id, secret };
    let json = match to_json(&request) {
      Ok(json) => json,
      Err(err) => return Ok(Err(Error::Json(err))),
    };
    let message = wrap::Message::Text(json);
    let response = self.0.send(message).await?;

    match response {
      Some(response) => match response {
        Ok(ControlMessage::AuthenticationMessage(authentication)) => {
          if authentication.status != AuthenticationStatus::Authorized {
            return Ok(Err(Error::Str("authentication not successful".into())))
          }
          Ok(Ok(()))
        },
        Ok(_) => Ok(Err(Error::Str(
          "server responded with an unexpected message".into(),
        ))),
        Err(()) => Ok(Err(Error::Str("failed to authenticate with server".into()))),
      },
      None => Ok(Err(Error::Str(
        "stream was closed before authorization message was received".into(),
      ))),
    }
  }

  /// Subscribe and listen to trade updates.
  async fn listen(&mut self) -> Result<Result<(), Error>, S::Error> {
    let streams = Streams::from([StreamType::TradeUpdates].as_ref());
    let request = Request::Listen(streams);
    let json = match to_json(&request) {
      Ok(json) => json,
      Err(err) => return Ok(Err(Error::Json(err))),
    };
    let message = wrap::Message::Text(json);
    let response = self.0.send(message).await?;

    match response {
      Some(response) => match response {
        Ok(ControlMessage::ListeningMessage(streams)) => {
          if !streams.streams.contains(&StreamType::TradeUpdates) {
            return Ok(Err(Error::Str(
              "server did not subscribe us to trade update stream".into(),
            )))
          }
          Ok(Ok(()))
        },
        Ok(_) => Ok(Err(Error::Str(
          "server responded with an unexpected message".into(),
        ))),
        Err(()) => Ok(Err(Error::Str(
          "failed to listen to trade update stream".into(),
        ))),
      },
      None => Ok(Err(Error::Str(
        "stream was closed before listen message was received".into(),
      ))),
    }
  }
}


type Stream = Map<Wrapper<WebSocketStream<MaybeTlsStream<TcpStream>>>, MapFn>;
type MapFn = fn(Result<wrap::Message, WebSocketError>) -> ParsedMessage;


/// A type used for requesting a subscription to the "trade_updates"
/// event stream.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TradeUpdates {}

impl EventStream for TradeUpdates {
  type Event = TradeUpdate;

  #[inline]
  fn stream() -> StreamType {
    StreamType::TradeUpdates
  }
}

#[async_trait(?Send)]
impl Subscribable for TradeUpdates {
  type Input = ApiInfo;
  type Subscription = Subscription<SplitSink<Stream, wrap::Message>>;
  type Stream = Fuse<MessageStream<SplitStream<Stream>, ParsedMessage>>;

  async fn connect(api_info: &Self::Input) -> Result<(Self::Stream, Self::Subscription), Error> {
    fn map(result: Result<wrap::Message, WebSocketError>) -> ParsedMessage {
      MessageResult::from(result.map(|message| match message {
        wrap::Message::Text(string) => json_from_str::<TradeMessage>(&string),
        wrap::Message::Binary(data) => json_from_slice::<TradeMessage>(&data),
      }))
    }

    let ApiInfo {
      base_url: url,
      key_id,
      secret,
    } = api_info;

    let stream = connect(url.clone()).await?.map(map as MapFn);
    let (send, recv) = stream.split();
    let (stream, subscription) = subscribe::subscribe(recv, send);
    let mut stream = stream.fuse();

    let mut subscription = Subscription(subscription);
    let authenticate = subscription
      .authenticate(key_id, secret)
      .boxed_local()
      .fuse();
    let () = subscribe::drive::<ParsedMessage, _, _>(authenticate, &mut stream)
      .await
      .map_err(|result| {
        result
          .map(|result| Error::Json(result.unwrap_err()))
          .map_err(Error::WebSocket)
          .unwrap_or_else(|err| err)
      })???;

    let listen = subscription.listen().boxed_local().fuse();
    let () = subscribe::drive::<ParsedMessage, _, _>(listen, &mut stream)
      .await
      .map_err(|result| {
        result
          .map(|result| Error::Json(result.unwrap_err()))
          .map_err(Error::WebSocket)
          .unwrap_or_else(|err| err)
      })???;

    Ok((stream, subscription))
  }
}


#[cfg(test)]
mod tests {
  use super::*;

  use futures::future::ok;
  use futures::pin_mut;
  use futures::TryStreamExt;

  use serde_json::from_str as json_from_str;

  use test_log::test;

  use url::Url;

  use crate::api::v2::order;
  use crate::api::v2::order_util::order_aapl;
  use crate::api::API_BASE_URL;
  use crate::Client;
  use crate::Error;


  /// Check that we can encode an authentication request correctly.
  #[test]
  fn encode_authentication_request() {
    let key_id = "some-key";
    let secret = "super-secret-secret";
    let expected = {
      r#"{"action":"authenticate","data":{"key_id":"some-key","secret_key":"super-secret-secret"}}"#
    };

    let request = Request::Authenticate { key_id, secret };
    let json = to_json(&request).unwrap();
    assert_eq!(json, expected)
  }

  /// Check that we can encode a listen request properly.
  #[test]
  fn encode_listen_request() {
    let expected = r#"{"action":"listen","data":{"streams":["trade_updates"]}}"#;
    let streams = Streams::from([StreamType::TradeUpdates].as_ref());
    let request = Request::Listen(streams);
    let json = to_json(&request).unwrap();
    assert_eq!(json, expected)
  }

  /// Verify that we can decode a trade update.
  #[test]
  fn decode_trade_update() {
    let json = r#"{
  "stream":"trade_updates","data":{
    "event":"new","execution_id":"11111111-2222-3333-4444-555555555555","order":{
      "asset_class":"us_equity","asset_id":"11111111-2222-3333-4444-555555555555",
      "canceled_at":null,"client_order_id":"11111111-2222-3333-4444-555555555555",
      "created_at":"2021-12-09T19:48:46.176628398Z","expired_at":null,
      "extended_hours":false,"failed_at":null,"filled_at":null,
      "filled_avg_price":null,"filled_qty":"0","hwm":null,
      "id":"11111111-2222-3333-4444-555555555555","legs":null,"limit_price":"1",
      "notional":null,"order_class":"simple","order_type":"limit","qty":"1",
      "replaced_at":null,"replaced_by":null,"replaces":null,"side":"buy",
      "status":"new","stop_price":null,"submitted_at":"2021-12-09T19:48:46.175261379Z",
      "symbol":"AAPL","time_in_force":"day","trail_percent":null,"trail_price":null,
      "type":"limit","updated_at":"2021-12-09T19:48:46.185346448Z"
    },"timestamp":"2021-12-09T19:48:46.182987144Z"
  }
}"#;
    let message = json_from_str::<TradeMessage>(json).unwrap();
    match message {
      TradeMessage::TradeUpdate(update) => {
        assert_eq!(update.event, TradeStatus::New);
        assert_eq!(update.order.side, order::Side::Buy);
      },
      _ => panic!("Decoded unexpected message variant: {:?}", message),
    }
  }

  /// Verify that we can decode a authentication control message.
  #[test]
  fn decode_authentication() {
    let json =
      { r#"{"stream":"authorization","data":{"status":"authorized","action":"authenticate"}}"# };
    let message = json_from_str::<TradeMessage>(json).unwrap();
    match message {
      TradeMessage::AuthenticationMessage(authentication) => {
        assert_eq!(authentication.status, AuthenticationStatus::Authorized);
      },
      _ => panic!("Decoded unexpected message variant: {:?}", message),
    }
  }

  /// Check that we can decode an authentication control message
  /// indicating an unsuccessful authorization.
  #[test]
  fn decode_unauthorized_authentication() {
    let json =
      { r#"{"stream":"authorization","data":{"status":"unauthorized","action":"listen"}}"# };
    let message = json_from_str::<TradeMessage>(json).unwrap();
    match message {
      TradeMessage::AuthenticationMessage(authentication) => {
        assert_eq!(authentication.status, AuthenticationStatus::Unauthorized);
      },
      _ => panic!("Decoded unexpected message variant: {:?}", message),
    }
  }

  /// Verify that we can decode a listening control message.
  #[test]
  fn decode_listening() {
    let json = r#"{"stream":"listening","data":{"streams":["trade_updates"]}}"#;

    let message = json_from_str::<TradeMessage>(json).unwrap();
    match message {
      TradeMessage::ListeningMessage(streams) => {
        assert_eq!(streams.streams, vec![StreamType::TradeUpdates]);
      },
      _ => panic!("Decoded unexpected message variant: {:?}", message),
    }
  }

  #[test(tokio::test)]
  async fn stream_trade_events() {
    // TODO: There may be something amiss here. If we don't cancel the
    //       order we never get an event about a new trade. That does
    //       not seem to be in our code, though, as the behavior is the
    //       same when streaming events using Alpaca's Python client.
    let api_info = ApiInfo::from_env().unwrap();
    let client = Client::new(api_info);
    let (stream, _subscription) = client.subscribe::<TradeUpdates>().await.unwrap();
    pin_mut!(stream);

    let order = order_aapl(&client).await.unwrap();
    client.issue::<order::Delete>(&order.id).await.unwrap();

    let trade = stream
      .try_filter_map(|res| {
        let trade = res.unwrap();
        ok(Some(trade))
      })
      // There could be other trades happening concurrently but we
      // are only interested in ones belonging to the order we
      // submitted as part of this test.
      .try_skip_while(|trade| ok(trade.order.id != order.id))
      .next()
      .await
      .unwrap()
      .unwrap();

    assert_eq!(order.id, trade.order.id);
    assert_eq!(order.asset_id, trade.order.asset_id);
    assert_eq!(order.symbol, trade.order.symbol);
    assert_eq!(order.asset_class, trade.order.asset_class);
    assert_eq!(order.type_, trade.order.type_);
    assert_eq!(order.side, trade.order.side);
    assert_eq!(order.time_in_force, trade.order.time_in_force);
  }

  #[test(tokio::test)]
  async fn stream_with_invalid_credentials() {
    let api_base = Url::parse(API_BASE_URL).unwrap();
    let api_info = ApiInfo {
      base_url: api_base,
      key_id: "invalid".to_string(),
      secret: "invalid-too".to_string(),
    };

    let client = Client::new(api_info);
    let result = client.subscribe::<TradeUpdates>().await;

    match result {
      Ok(_) => panic!("operation succeeded unexpectedly"),
      Err(Error::Str(ref e)) if e == "authentication not successful" => (),
      Err(e) => panic!("received unexpected error: {}", e),
    }
  }
}
