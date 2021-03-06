use std::future::Future;
use std::time;

use actix_codec::{AsyncRead, AsyncWrite};
use bytes::Bytes;
use futures_util::future::poll_fn;
use h2::{
    client::{Builder, Connection, SendRequest},
    SendStream,
};
use http::header::{HeaderValue, CONNECTION, CONTENT_LENGTH, TRANSFER_ENCODING};
use http::{request::Request, Method, Version};

use crate::body::{BodySize, MessageBody};
use crate::header::HeaderMap;
use crate::message::{RequestHeadType, ResponseHead};
use crate::payload::Payload;

use super::config::ConnectorConfig;
use super::connection::ConnectionType;
use super::error::SendRequestError;
use super::pool::Acquired;
use crate::client::connection::H2Connection;

pub(crate) async fn send_request<T, B>(
    mut io: H2Connection,
    head: RequestHeadType,
    body: B,
    created: time::Instant,
    acquired: Acquired<T>,
) -> Result<(ResponseHead, Payload), SendRequestError>
where
    T: AsyncRead + AsyncWrite + Unpin + 'static,
    B: MessageBody,
{
    trace!("Sending client request: {:?} {:?}", head, body.size());

    let head_req = head.as_ref().method == Method::HEAD;
    let length = body.size();
    let eof = matches!(
        length,
        BodySize::None | BodySize::Empty | BodySize::Sized(0)
    );

    let mut req = Request::new(());
    *req.uri_mut() = head.as_ref().uri.clone();
    *req.method_mut() = head.as_ref().method.clone();
    *req.version_mut() = Version::HTTP_2;

    let mut skip_len = true;
    // let mut has_date = false;

    // Content length
    let _ = match length {
        BodySize::None => None,
        BodySize::Stream => {
            skip_len = false;
            None
        }
        BodySize::Empty => req
            .headers_mut()
            .insert(CONTENT_LENGTH, HeaderValue::from_static("0")),
        BodySize::Sized(len) => {
            let mut buf = itoa::Buffer::new();

            req.headers_mut().insert(
                CONTENT_LENGTH,
                HeaderValue::from_str(buf.format(len)).unwrap(),
            )
        }
    };

    // Extracting extra headers from RequestHeadType. HeaderMap::new() does not allocate.
    let (head, extra_headers) = match head {
        RequestHeadType::Owned(head) => (RequestHeadType::Owned(head), HeaderMap::new()),
        RequestHeadType::Rc(head, extra_headers) => (
            RequestHeadType::Rc(head, None),
            extra_headers.unwrap_or_else(HeaderMap::new),
        ),
    };

    // merging headers from head and extra headers.
    let headers = head
        .as_ref()
        .headers
        .iter()
        .filter(|(name, _)| !extra_headers.contains_key(*name))
        .chain(extra_headers.iter());

    // copy headers
    for (key, value) in headers {
        match *key {
            // TODO: consider skipping other headers according to:
            //       https://tools.ietf.org/html/rfc7540#section-8.1.2.2
            // omit HTTP/1.x only headers
            CONNECTION | TRANSFER_ENCODING => continue,
            CONTENT_LENGTH if skip_len => continue,
            // DATE => has_date = true,
            _ => {}
        }
        req.headers_mut().append(key, value.clone());
    }

    let res = poll_fn(|cx| io.poll_ready(cx)).await;
    if let Err(e) = res {
        release(io, acquired, created, e.is_io());
        return Err(SendRequestError::from(e));
    }

    let resp = match io.send_request(req, eof) {
        Ok((fut, send)) => {
            release(io, acquired, created, false);

            if !eof {
                send_body(body, send).await?;
            }
            fut.await.map_err(SendRequestError::from)?
        }
        Err(e) => {
            release(io, acquired, created, e.is_io());
            return Err(e.into());
        }
    };

    let (parts, body) = resp.into_parts();
    let payload = if head_req { Payload::None } else { body.into() };

    let mut head = ResponseHead::new(parts.status);
    head.version = parts.version;
    head.headers = parts.headers.into();
    Ok((head, payload))
}

async fn send_body<B: MessageBody>(
    body: B,
    mut send: SendStream<Bytes>,
) -> Result<(), SendRequestError> {
    let mut buf = None;
    actix_rt::pin!(body);
    loop {
        if buf.is_none() {
            match poll_fn(|cx| body.as_mut().poll_next(cx)).await {
                Some(Ok(b)) => {
                    send.reserve_capacity(b.len());
                    buf = Some(b);
                }
                Some(Err(e)) => return Err(e.into()),
                None => {
                    if let Err(e) = send.send_data(Bytes::new(), true) {
                        return Err(e.into());
                    }
                    send.reserve_capacity(0);
                    return Ok(());
                }
            }
        }

        match poll_fn(|cx| send.poll_capacity(cx)).await {
            None => return Ok(()),
            Some(Ok(cap)) => {
                let b = buf.as_mut().unwrap();
                let len = b.len();
                let bytes = b.split_to(std::cmp::min(cap, len));

                if let Err(e) = send.send_data(bytes, false) {
                    return Err(e.into());
                } else {
                    if !b.is_empty() {
                        send.reserve_capacity(b.len());
                    } else {
                        buf = None;
                    }
                    continue;
                }
            }
            Some(Err(e)) => return Err(e.into()),
        }
    }
}

/// release SendRequest object
fn release<T: AsyncRead + AsyncWrite + Unpin + 'static>(
    io: H2Connection,
    acquired: Acquired<T>,
    created: time::Instant,
    close: bool,
) {
    if close {
        acquired.close(ConnectionType::H2(io));
    } else {
        acquired.release(ConnectionType::H2(io), created);
    }
}

pub(crate) fn handshake<Io>(
    io: Io,
    config: &ConnectorConfig,
) -> impl Future<Output = Result<(SendRequest<Bytes>, Connection<Io, Bytes>), h2::Error>>
where
    Io: AsyncRead + AsyncWrite + Unpin + 'static,
{
    let mut builder = Builder::new();
    builder
        .initial_window_size(config.stream_window_size)
        .initial_connection_window_size(config.conn_window_size)
        .enable_push(false);
    builder.handshake(io)
}
