use std::{path::PathBuf, sync::Arc};

use dashmap::DashMap;
use nix::sys::signal::Signal;
use shim_protos::proto::{
    task_server::Task, CreateTaskRequest, CreateTaskResponse, DeleteRequest, DeleteResponse,
    KillRequest, ShutdownRequest, StartRequest, StartResponse, WaitRequest, WaitResponse,
};
use tonic::{Request, Response, Status};
use tracing::debug;

use crate::{
    container::{Container, Status as ContainerStatus},
    utils::ExitSignal,
};

pub struct TaskService {
    pub runtime: PathBuf,
    pub containers: Arc<DashMap<String, Container>>,
    pub exit_signal: Arc<ExitSignal>,
}

impl TaskService {
    pub fn new(runtime: PathBuf, exit_signal: Arc<ExitSignal>) -> Self {
        Self {
            runtime,
            containers: Arc::new(DashMap::new()),
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
        let request = request.into_inner();
        if self.containers.contains_key(&request.id) {
            return Err(Status::new(
                tonic::Code::AlreadyExists,
                "Container already exists",
            ));
        }
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
        self.containers.insert(request.id, container);
        Ok(Response::new(CreateTaskResponse { pid }))
    }

    async fn start(
        &self,
        request: Request<StartRequest>,
    ) -> Result<Response<StartResponse>, Status> {
        debug!("Starting container");
        let request = request.into_inner();
        let container = self
            .containers
            .get(&request.id)
            .ok_or_else(|| Status::new(tonic::Code::NotFound, "Container not found"))?;
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
        let container = self
            .containers
            .get(&request.id)
            .ok_or_else(|| Status::new(tonic::Code::NotFound, "Container not found"))?;
        if let Err(err) = container.delete(&self.runtime).await {
            return Err(Status::new(
                tonic::Code::Internal,
                format!("Failed to delete container: {}", err),
            ));
        }
        let pid = container.pid().await as u32;
        drop(container);
        self.containers.remove(&request.id);
        Ok(Response::new(DeleteResponse { pid }))
    }

    async fn wait(&self, request: Request<WaitRequest>) -> Result<Response<WaitResponse>, Status> {
        debug!("Waiting for container");
        let request = request.into_inner();
        let container = self
            .containers
            .get(&request.id)
            .ok_or_else(|| Status::new(tonic::Code::NotFound, "Container not found"))?;
        let status = container.status().await;
        if status != ContainerStatus::RUNNING && status != ContainerStatus::CREATED {
            return Ok(Response::new(WaitResponse {
                exit_status: container.exit_code().await as u32,
                exited_at: container.exited_at().await,
            }));
        }
        let mut rx = container.wait_channel().await;
        if rx.recv().await.is_none() {
            return Err(Status::new(
                tonic::Code::Aborted,
                "Container exited unexpectedly",
            ));
        }
        Ok(Response::new(WaitResponse {
            exit_status: container.exit_code().await as u32,
            exited_at: container.exited_at().await,
        }))
    }

    async fn kill(&self, request: Request<KillRequest>) -> Result<Response<()>, Status> {
        debug!("Killing container");
        let request = request.into_inner();
        let container = self
            .containers
            .get(&request.id)
            .ok_or_else(|| Status::new(tonic::Code::NotFound, "Container not found"))?;
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
        for container in self.containers.iter() {
            // Kills all containers so that all `TaskService::wait` calls return and Tonic can shutdown.
            if let Err(err) = container.delete(&self.runtime).await {
                return Err(Status::new(
                    tonic::Code::Internal,
                    format!("Failed to delete container: {}", err),
                ));
            }
        }
        self.containers.clear();
        self.exit_signal.signal();
        Ok(Response::new(()))
    }
}
