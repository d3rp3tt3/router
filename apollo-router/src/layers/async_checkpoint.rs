//! Asynchronous Checkpoint [`Layer`].
//!
//! Provides a general mechanism for controlling the flow of a request. Useful in any situation
//! where the caller wishes to provide control flow for a request.
//!
//! If the evaluated closure succeeds then the request is passed onto the next service in the
//! chain of responsibilities. If it fails, then the control flow is broken a response is passed
//! back to the invoking service.

use std::marker::PhantomData;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::sync::Arc;

use futures::future::BoxFuture;
use futures::Future;
use tower::BoxError;
use tower::Layer;
use tower::Service;
use tower::ServiceExt;

/// [`Layer`] for Asynchronous Checkpoints.
#[allow(clippy::type_complexity)]
pub struct AsyncCheckpointLayer<S, Fut, Request>
where
    S: Service<Request, Error = BoxError> + Clone + Send + 'static,
    Fut: Future<Output = Result<ControlFlow<<S as Service<Request>>::Response, Request>, BoxError>>,
{
    checkpoint_fn: Arc<Pin<Box<dyn Fn(Request) -> Fut + Send + Sync + 'static>>>,
    phantom: PhantomData<S>, // XXX: The compiler can't detect that S is used in the Future...
}

impl<S, Fut, Request> AsyncCheckpointLayer<S, Fut, Request>
where
    S: Service<Request, Error = BoxError> + Clone + Send + 'static,
    Fut: Future<Output = Result<ControlFlow<<S as Service<Request>>::Response, Request>, BoxError>>,
{
    /// Create an `AsyncCheckpointLayer` from a function that takes a Service Request and returns a `ControlFlow`
    pub fn new<F>(checkpoint_fn: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
    {
        Self {
            checkpoint_fn: Arc::new(Box::pin(checkpoint_fn)),
            phantom: PhantomData,
        }
    }
}

impl<S, Fut, Request> Layer<S> for AsyncCheckpointLayer<S, Fut, Request>
where
    S: Service<Request, Error = BoxError> + Clone + Send + 'static,
    <S as Service<Request>>::Future: Send,
    Request: Send + 'static,
    <S as Service<Request>>::Response: Send + 'static,
    Fut: Future<Output = Result<ControlFlow<<S as Service<Request>>::Response, Request>, BoxError>>,
{
    type Service = AsyncCheckpointService<S, Fut, Request>;

    fn layer(&self, service: S) -> Self::Service {
        AsyncCheckpointService {
            checkpoint_fn: Arc::clone(&self.checkpoint_fn),
            inner: service,
        }
    }
}

/// [`Service`] for Asynchronous Checkpoints.
#[allow(clippy::type_complexity)]
pub struct AsyncCheckpointService<S, Fut, Request>
where
    Request: Send + 'static,
    S: Service<Request, Error = BoxError> + Clone + Send + 'static,
    <S as Service<Request>>::Response: Send + 'static,
    <S as Service<Request>>::Future: Send + 'static,
    Fut: Future<Output = Result<ControlFlow<<S as Service<Request>>::Response, Request>, BoxError>>,
{
    inner: S,
    checkpoint_fn: Arc<Pin<Box<dyn Fn(Request) -> Fut + Send + Sync + 'static>>>,
}

impl<S, Fut, Request> AsyncCheckpointService<S, Fut, Request>
where
    Request: Send + 'static,
    S: Service<Request, Error = BoxError> + Clone + Send + 'static,
    <S as Service<Request>>::Response: Send + 'static,
    <S as Service<Request>>::Future: Send + 'static,
    Fut: Future<Output = Result<ControlFlow<<S as Service<Request>>::Response, Request>, BoxError>>,
{
    /// Create an `AsyncCheckpointLayer` from a function that takes a Service Request and returns a `ControlFlow`
    pub fn new<F>(checkpoint_fn: F, service: S) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
    {
        Self {
            checkpoint_fn: Arc::new(Box::pin(checkpoint_fn)),
            inner: service,
        }
    }
}

impl<S, Fut, Request> Service<Request> for AsyncCheckpointService<S, Fut, Request>
where
    Request: Send + 'static,
    S: Service<Request, Error = BoxError> + Clone + Send + 'static,
    <S as Service<Request>>::Response: Send + 'static,
    <S as Service<Request>>::Future: Send + 'static,
    Fut: Future<Output = Result<ControlFlow<<S as Service<Request>>::Response, Request>, BoxError>>
        + Send
        + 'static,
{
    type Response = <S as Service<Request>>::Response;

    type Error = BoxError;

    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let checkpoint_fn = Arc::clone(&self.checkpoint_fn);
        let inner = self.inner.clone();
        Box::pin(async move {
            match (checkpoint_fn)(req).await {
                Ok(ControlFlow::Break(response)) => Ok(response),
                Ok(ControlFlow::Continue(request)) => inner.oneshot(request).await,
                Err(error) => Err(error),
            }
        })
    }
}

#[cfg(test)]
mod async_checkpoint_tests {
    use tower::BoxError;
    use tower::Layer;
    use tower::ServiceBuilder;
    use tower::ServiceExt;

    use super::*;
    use crate::layers::ServiceBuilderExt;
    use crate::plugin::test::MockExecutionService;
    use crate::ExecutionRequest;
    use crate::ExecutionResponse;

    #[tokio::test]
    async fn test_service_builder() {
        let expected_label = "from_mock_service";

        let mut execution_service = MockExecutionService::new();
        execution_service.expect_clone().return_once(move || {
            let mut execution_service = MockExecutionService::new();
            execution_service.expect_call().times(1).returning(
                move |_req: crate::ExecutionRequest| {
                    Ok(ExecutionResponse::fake_builder()
                        .label(expected_label.to_string())
                        .build())
                },
            );

            execution_service
        });

        let service_stack = ServiceBuilder::new()
            .checkpoint_async(|req: crate::ExecutionRequest| async {
                Ok(ControlFlow::Continue(req))
            })
            .service(execution_service);

        let request = ExecutionRequest::fake_builder().build();

        let actual_label = service_stack
            .oneshot(request)
            .await
            .unwrap()
            .next_response()
            .await
            .unwrap()
            .label
            .unwrap();

        assert_eq!(actual_label, expected_label)
    }

    #[tokio::test]
    async fn test_continue() {
        let expected_label = "from_mock_service";
        let mut router_service = MockExecutionService::new();

        router_service.expect_clone().return_once(move || {
            let mut router_service = MockExecutionService::new();
            router_service
                .expect_call()
                .times(1)
                .returning(move |_req| {
                    Ok(ExecutionResponse::fake_builder()
                        .label(expected_label.to_string())
                        .build())
                });
            router_service
        });

        let service_stack =
            AsyncCheckpointLayer::new(|req| async { Ok(ControlFlow::Continue(req)) })
                .layer(router_service);

        let request = ExecutionRequest::fake_builder().build();

        let actual_label = service_stack
            .oneshot(request)
            .await
            .unwrap()
            .next_response()
            .await
            .unwrap()
            .label
            .unwrap();

        assert_eq!(actual_label, expected_label)
    }

    #[tokio::test]
    async fn test_return() {
        let expected_label = "returned_before_mock_service";
        let mut router_service = MockExecutionService::new();
        router_service
            .expect_clone()
            .return_once(MockExecutionService::new);

        let service_stack = AsyncCheckpointLayer::new(|_req| async {
            Ok(ControlFlow::Break(
                ExecutionResponse::fake_builder()
                    .label("returned_before_mock_service".to_string())
                    .build(),
            ))
        })
        .layer(router_service);

        let request = ExecutionRequest::fake_builder().build();

        let actual_label = service_stack
            .oneshot(request)
            .await
            .unwrap()
            .next_response()
            .await
            .unwrap()
            .label
            .unwrap();

        assert_eq!(actual_label, expected_label)
    }

    #[tokio::test]
    async fn test_error() {
        let expected_error = "checkpoint_error";
        let mut router_service = MockExecutionService::new();
        router_service
            .expect_clone()
            .return_once(MockExecutionService::new);

        let service_stack =
            AsyncCheckpointLayer::new(
                move |_req| async move { Err(BoxError::from(expected_error)) },
            )
            .layer(router_service);

        let request = ExecutionRequest::fake_builder().build();

        let actual_error = service_stack
            .oneshot(request)
            .await
            .map(|_| unreachable!())
            .unwrap_err()
            .to_string();

        assert_eq!(actual_error, expected_error)
    }
}
