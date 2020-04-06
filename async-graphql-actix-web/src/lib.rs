//! Integrate `async-graphql` to `actix-web`

#![warn(missing_docs)]

mod session;

use crate::session::WsSession;
use actix_multipart::Multipart;
use actix_web::http::{header, HeaderMap, Method};
use actix_web::web::{BytesMut, Payload};
use actix_web::{web, FromRequest, HttpRequest, HttpResponse, Responder};
use actix_web_actors::ws;
use async_graphql::http::{GQLRequest, GQLResponse};
use async_graphql::{ObjectType, QueryBuilder, Schema, SubscriptionType};
use bytes::Bytes;
use futures::StreamExt;
use mime::Mime;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

type BoxOnRequestFn<Query, Mutation, Subscription> = Arc<
    dyn for<'a> Fn(
        &HttpRequest,
        QueryBuilder<Query, Mutation, Subscription>,
    ) -> QueryBuilder<Query, Mutation, Subscription>,
>;

/// Actix-web handler builder
pub struct HandlerBuilder<Query, Mutation, Subscription> {
    schema: Schema<Query, Mutation, Subscription>,
    max_file_size: usize,
    max_file_count: usize,
    enable_subscription: bool,
    enable_ui: Option<(String, Option<String>)>,
    on_request: Option<BoxOnRequestFn<Query, Mutation, Subscription>>,
}

impl<Query, Mutation, Subscription> HandlerBuilder<Query, Mutation, Subscription>
where
    Query: ObjectType + Send + Sync + 'static,
    Mutation: ObjectType + Send + Sync + 'static,
    Subscription: SubscriptionType + Send + Sync + 'static,
{
    /// Create an HTTP handler builder
    pub fn new(schema: Schema<Query, Mutation, Subscription>) -> Self {
        Self {
            schema,
            max_file_size: 1024 * 1024 * 2,
            max_file_count: 9,
            enable_subscription: false,
            enable_ui: None,
            on_request: None,
        }
    }

    /// Set the maximum file size for upload, default 2M bytes.
    pub fn max_file_size(self, size: usize) -> Self {
        Self {
            max_file_size: size,
            ..self
        }
    }

    /// Set the maximum files count for upload, default 9.
    pub fn max_files(self, count: usize) -> Self {
        Self {
            max_file_count: count,
            ..self
        }
    }

    /// Enable GraphQL playground
    ///
    /// 'endpoint' is the endpoint of the GraphQL Request.
    /// 'subscription_endpoint' is the endpoint of the GraphQL Subscription.
    pub fn enable_ui(self, endpoint: &str, subscription_endpoint: Option<&str>) -> Self {
        Self {
            enable_ui: Some((
                endpoint.to_string(),
                subscription_endpoint.map(|s| s.to_string()),
            )),
            ..self
        }
    }

    /// Enable GraphQL Subscription.
    pub fn enable_subscription(self) -> Self {
        Self {
            enable_subscription: true,
            ..self
        }
    }

    /// When a new request arrives, you can use this closure to append your own data to the `QueryBuilder`.
    pub fn on_request<
        F: for<'a> Fn(
                &HttpRequest,
                QueryBuilder<Query, Mutation, Subscription>,
            ) -> QueryBuilder<Query, Mutation, Subscription>
            + 'static,
    >(
        self,
        f: F,
    ) -> Self {
        Self {
            on_request: Some(Arc::new(f)),
            ..self
        }
    }

    /// Create an HTTP handler.
    pub fn build(
        self,
    ) -> impl Fn(
        HttpRequest,
        Payload,
    ) -> Pin<Box<dyn Future<Output = actix_web::Result<HttpResponse>>>>
           + Clone
           + 'static {
        let schema = self.schema.clone();
        let max_file_size = self.max_file_size;
        let max_file_count = self.max_file_count;
        let enable_ui = self.enable_ui;
        let enable_subscription = self.enable_subscription;
        let on_request = self.on_request;

        move |req: HttpRequest, payload: Payload| {
            let schema = schema.clone();
            let enable_ui = enable_ui.clone();
            let on_request = on_request.clone();

            Box::pin(async move {
                if req.method() == Method::GET {
                    if enable_subscription {
                        if let Some(s) = req.headers().get(header::UPGRADE) {
                            if let Ok(s) = s.to_str() {
                                if s.to_ascii_lowercase().contains("websocket") {
                                    return ws::start_with_protocols(
                                        WsSession::new(schema.clone()),
                                        &["graphql-ws"],
                                        &req,
                                        payload,
                                    );
                                }
                            }
                        }
                    }

                    if let Some((endpoint, subscription_endpoint)) = &enable_ui {
                        return Ok(HttpResponse::Ok()
                            .content_type("text/html; charset=utf-8")
                            .body(async_graphql::http::playground_source(
                                endpoint,
                                subscription_endpoint.as_deref(),
                            )));
                    }
                }

                if req.method() == Method::POST {
                    handle_request(
                        &schema,
                        max_file_size,
                        max_file_count,
                        req,
                        payload,
                        on_request.as_ref(),
                    )
                    .await
                } else {
                    Ok(HttpResponse::MethodNotAllowed().finish())
                }
            })
        }
    }
}

async fn handle_request<Query, Mutation, Subscription>(
    schema: &Schema<Query, Mutation, Subscription>,
    max_file_size: usize,
    max_file_count: usize,
    req: HttpRequest,
    mut payload: Payload,
    on_request: Option<&BoxOnRequestFn<Query, Mutation, Subscription>>,
) -> actix_web::Result<HttpResponse>
where
    Query: ObjectType + Send + Sync + 'static,
    Mutation: ObjectType + Send + Sync + 'static,
    Subscription: SubscriptionType + Send + Sync + 'static,
{
    if let Ok(ct) = get_content_type(req.headers()) {
        if ct.essence_str() == mime::MULTIPART_FORM_DATA {
            let mut multipart = Multipart::from_request(&req, &mut payload.0).await?;

            // read operators
            let gql_request = {
                let data = read_multipart(&mut multipart, "operations").await?;
                serde_json::from_slice::<GQLRequest>(&data)
                    .map_err(actix_web::error::ErrorBadRequest)?
            };

            // read map
            let mut map = {
                let data = read_multipart(&mut multipart, "map").await?;
                serde_json::from_slice::<HashMap<String, Vec<String>>>(&data)
                    .map_err(actix_web::error::ErrorBadRequest)?
            };

            let mut builder = match gql_request.into_query_builder(schema) {
                Ok(builder) => builder,
                Err(err) => return Ok(web::Json(GQLResponse(Err(err))).respond_to(&req).await?),
            };

            if let Some(on_request) = on_request {
                builder = on_request(&req, builder);
            }

            if !builder.is_upload() {
                return Err(actix_web::error::ErrorBadRequest(
                    "It's not an upload operation",
                ));
            }

            // read files
            let mut file_count = 0;
            while let Some(field) = multipart.next().await {
                let mut field = field?;
                if let Some(content_disposition) = field.content_disposition() {
                    if let (Some(name), Some(filename)) = (
                        content_disposition.get_name(),
                        content_disposition.get_filename(),
                    ) {
                        if let Some(var_paths) = map.remove(name) {
                            let content_type = field.content_type().to_string();
                            let mut data = BytesMut::new();
                            while let Some(part) = field.next().await {
                                let part = part.map_err(actix_web::error::ErrorBadRequest)?;
                                data.extend(&part);

                                if data.len() > max_file_size {
                                    return Err(actix_web::error::ErrorPayloadTooLarge(
                                        "payload too large",
                                    ));
                                }
                            }

                            let data = data.freeze();

                            for var_path in var_paths {
                                builder.set_upload(
                                    &var_path,
                                    filename,
                                    Some(&content_type),
                                    data.clone(),
                                );
                            }
                            file_count += 1;
                            if file_count > max_file_count {
                                return Err(actix_web::error::ErrorPayloadTooLarge(
                                    "payload too large",
                                ));
                            }
                        } else {
                            return Err(actix_web::error::ErrorBadRequest("bad request"));
                        }
                    } else {
                        return Err(actix_web::error::ErrorBadRequest("bad request"));
                    }
                } else {
                    return Err(actix_web::error::ErrorBadRequest("bad request"));
                }
            }

            if !map.is_empty() {
                return Err(actix_web::error::ErrorBadRequest("missing files"));
            }

            Ok(web::Json(GQLResponse(builder.execute().await))
                .respond_to(&req)
                .await?)
        } else if ct.essence_str() == mime::APPLICATION_JSON {
            let gql_request = web::Json::<GQLRequest>::from_request(&req, &mut payload.0)
                .await?
                .into_inner();
            let mut builder = match gql_request.into_query_builder(schema) {
                Ok(builder) => builder,
                Err(err) => return Ok(web::Json(GQLResponse(Err(err))).respond_to(&req).await?),
            };
            if let Some(on_request) = on_request {
                builder = on_request(&req, builder);
            }
            let mut cache_control = builder.cache_control().value();
            let gql_resp = builder.execute().await;
            if gql_resp.is_err() {
                cache_control = None;
            }
            let mut resp = web::Json(GQLResponse(gql_resp)).respond_to(&req).await?;
            if let Some(cache_control) = cache_control {
                resp.headers_mut().insert(
                    header::CACHE_CONTROL,
                    header::HeaderValue::from_str(&cache_control).unwrap(),
                );
            }
            Ok(resp)
        } else {
            Ok(HttpResponse::UnsupportedMediaType().finish())
        }
    } else {
        Ok(HttpResponse::UnsupportedMediaType().finish())
    }
}

fn get_content_type(headers: &HeaderMap) -> actix_web::Result<Mime> {
    if let Some(content_type) = headers.get(header::CONTENT_TYPE) {
        if let Ok(content_type) = content_type.to_str() {
            if let Ok(ct) = content_type.parse::<Mime>() {
                return Ok(ct);
            }
        }
    }
    Err(actix_web::error::ErrorUnsupportedMediaType(
        "unsupported media type",
    ))
}

async fn read_multipart(multipart: &mut Multipart, name: &str) -> actix_web::Result<Bytes> {
    let data = match multipart.next().await {
        Some(Ok(mut field)) => {
            if let Some(content_disposition) = field.content_disposition() {
                if let Some(current_name) = content_disposition.get_name() {
                    if current_name != name {
                        return Err(actix_web::error::ErrorBadRequest(format!(
                            "expect \"{}\"",
                            name
                        )));
                    }

                    let mut data = BytesMut::new();
                    while let Some(part) = field.next().await {
                        let part = part.map_err(actix_web::error::ErrorBadRequest)?;
                        data.extend(&part);
                    }
                    data
                } else {
                    return Err(actix_web::error::ErrorBadRequest("missing \"operations\""));
                }
            } else {
                return Err(actix_web::error::ErrorBadRequest("bad request"));
            }
        }
        Some(Err(err)) => return Err(err.into()),
        None => return Err(actix_web::error::ErrorBadRequest("bad request")),
    };
    Ok(data.freeze())
}
