use std::{path::PathBuf, sync::Arc};

use nix::sys::signal::Signal;
use shim_protos::proto::{
    task_server::Task, CreateTaskRequest, CreateTaskResponse, DeleteRequest, DeleteResponse,
    KillRequest, ShutdownRequest, StartRequest, StartResponse, WaitRequest, WaitResponse,
};
use tokio::sync::RwLock;
use tonic::{Request, Response, Status};
use tracing::debug;

use crate::{
    container::{Container, Status as ContainerStatus},
    utils::ExitSignal,
};

pub struct TaskService {
    pub runtime: PathBuf,
    pub container: Arc<RwLock<Option<Container>>>,
    pub exit_signal: Arc<ExitSignal>,
}

impl TaskService {
    pub fn new(runtime: PathBuf, exit_signal: Arc<ExitSignal>) -> Self {
        Self {
            runtime,
            container: Arc::new(RwLock::new(None)),
            exit_signal,
        }
    }
}

#[tonic::async_trait]
impl Task for TaskService {
    async fn create(
        &self,
        request: Request<CreateTaskRequest>,
    ) -> Result<Response<CreateTaskResponse>, Status> {
        debug!("Creating container");
        if self.container.read().await.is_some() {
            return Err(Status::new(
                tonic::Code::AlreadyExists,
                "Container already exists",
            ));
        }
        let request = request.into_inner();
        let container = Container::new(
            &request.id,
            &request.bundle.into(),
            &request.stdout.into(),
            &request.stderr.into(),
        );
        if let Err(err) = container.create(&self.runtime).await {
            return Err(Status::new(
                tonic::Code::Internal,
                format!("Failed to create container: {}", err),
            ));
        }
        let pid = container.pid().await as u32;
        self.container.write().await.replace(container);
        Ok(Response::new(CreateTaskResponse { pid }))
    }

    async fn start(
        &self,
        request: Request<StartRequest>,
    ) -> Result<Response<StartResponse>, Status> {
        debug!("Starting container");
        let request = request.into_inner();
        let container_guard = self.container.read().await;
        let container = match container_guard.as_ref() {
            Some(container) if container.id == request.id => container,
            Some(_) | None => {
                return Err(Status::new(tonic::Code::NotFound, "Container not found"))
            }
        };
        if let Err(err) = container.start(&self.runtime).await {
            return Err(Status::new(
                tonic::Code::Internal,
                format!("Failed to start container: {}", err),
            ));
        }
        let pid = container.pid().await as u32;
        Ok(Response::new(StartResponse { pid }))
    }

    async fn delete(
        &self,
        request: Request<DeleteRequest>,
    ) -> Result<Response<DeleteResponse>, Status> {
        debug!("Deleting container");
        let request = request.into_inner();
        let container_guard = self.container.read().await;
        let container = match container_guard.as_ref() {
            Some(container) if container.id == request.id => container,
            Some(_) | None => {
                return Err(Status::new(tonic::Code::NotFound, "Container not found"))
            }
        };
        if let Err(err) = container.delete(&self.runtime).await {
            return Err(Status::new(
                tonic::Code::Internal,
                format!("Failed to delete container: {}", err),
            ));
        }
        let pid = container.pid().await as u32;
        drop(container_guard);
        self.container.write().await.take();
        Ok(Response::new(DeleteResponse { pid }))
    }

    async fn wait(&self, request: Request<WaitRequest>) -> Result<Response<WaitResponse>, Status> {
        debug!("Waiting for container");
        let request = request.into_inner();
        let container_guard = self.container.read().await;
        let container = match container_guard.as_ref() {
            Some(container) if container.id == request.id => container,
            Some(_) | None => {
                return Err(Status::new(tonic::Code::NotFound, "Container not found"))
            }
        };
        let status = container.status().await;
        if status != ContainerStatus::RUNNING && status != ContainerStatus::CREATED {
            return Ok(Response::new(WaitResponse {
                exit_status: container.exit_code().await as u32,
                exited_at: container.exited_at().await,
            }));
        }
        let mut rx = container.wait_channel().await;
        drop(container_guard);
        if rx.recv().await.is_none() {
            return Err(Status::new(
                tonic::Code::Aborted,
                "Container exited unexpectedly",
            ));
        }
        let container_guard = self.container.read().await;
        let container = match container_guard.as_ref() {
            Some(container) if container.id == request.id => container,
            Some(_) | None => {
                return Err(Status::new(
                    tonic::Code::NotFound,
                    "Container no longer exists",
                ))
            }
        };
        Ok(Response::new(WaitResponse {
            exit_status: container.exit_code().await as u32,
            exited_at: container.exited_at().await,
        }))
    }

    async fn kill(&self, request: Request<KillRequest>) -> Result<Response<()>, Status> {
        debug!("Killing container");
        let request = request.into_inner();
        let container_guard = self.container.read().await;
        let container = match container_guard.as_ref() {
            Some(container) if container.id == request.id => container,
            Some(_) | None => {
                return Err(Status::new(tonic::Code::NotFound, "Container not found"))
            }
        };
        let signal = match Signal::try_from(request.signal as i32) {
            Ok(signal) => signal,
            Err(err) => {
                return Err(Status::new(
                    tonic::Code::InvalidArgument,
                    format!("Invalid signal: {}", err),
                ))
            }
        };
        if let Err(err) = container.kill(signal).await {
            return Err(Status::new(
                tonic::Code::Internal,
                format!("Failed to kill container: {}", err),
            ));
        }
        Ok(Response::new(()))
    }

    async fn shutdown(&self, _request: Request<ShutdownRequest>) -> Result<Response<()>, Status> {
        debug!("Shutting down container");
        if let Some(container) = self.container.read().await.as_ref() {
            // Kill the container so that all `TaskService::wait` calls return and Tonic can shutdown.
            if let Err(err) = container.kill(Signal::SIGTERM).await {
                return Err(Status::new(
                    tonic::Code::Internal,
                    format!("Failed to kill container: {}", err),
                ));
            }
            if let Err(err) = container.delete(&self.runtime).await {
                return Err(Status::new(
                    tonic::Code::Internal,
                    format!("Failed to delete container: {}", err),
                ));
            }
        }
        self.container.write().await.take();
        self.exit_signal.signal();
        Ok(Response::new(()))
    }
}
