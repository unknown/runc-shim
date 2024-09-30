use std::{path::PathBuf, sync::Arc};

use nix::sys::signal::{kill, Signal};
use shim_protos::proto::{
    task_server::Task, CreateTaskRequest, CreateTaskResponse, DeleteRequest, DeleteResponse,
    ShutdownRequest, StartRequest, StartResponse, WaitRequest, WaitResponse,
};
use tokio::sync::{mpsc, Mutex};
use tonic::{Request, Response, Status};
use tracing::{debug, error};

use crate::{container::Container, signal::handle_signals, utils::ExitSignal};

pub struct TaskService {
    runtime: PathBuf,
    container: Arc<Mutex<Option<Container>>>,
    wait_channels: Arc<Mutex<Vec<mpsc::UnboundedSender<i32>>>>,
    exit_signal: Arc<ExitSignal>,
}

impl TaskService {
    pub fn new(runtime: PathBuf, exit_signal: Arc<ExitSignal>) -> Self {
        Self {
            runtime,
            container: Arc::new(Mutex::new(None)),
            wait_channels: Arc::new(Mutex::new(Vec::new())),
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
        let mut container_guard = self.container.lock().await;
        if container_guard.is_some() {
            return Err(Status::new(
                tonic::Code::AlreadyExists,
                "Container already exists",
            ));
        }
        let request = request.into_inner();
        let mut container = match Container::new(
            &request.id,
            &request.bundle.into(),
            &request.stdout.into(),
            &request.stderr.into(),
        ) {
            Ok(container) => container,
            Err(err) => {
                return Err(Status::new(
                    tonic::Code::Internal,
                    format!("Failed to initialize container: {}", err),
                ))
            }
        };
        if let Err(err) = container.create(&self.runtime).await {
            return Err(Status::new(
                tonic::Code::Internal,
                format!("Failed to create container: {}", err),
            ));
        }
        let pid = container.pid().unwrap();
        let raw_pid = pid.as_raw() as u32;
        *container_guard = Some(container);
        let wait_channels_clone = self.wait_channels.clone();
        tokio::spawn(async move { handle_signals(pid, wait_channels_clone).await });
        Ok(Response::new(CreateTaskResponse { pid: raw_pid }))
    }

    async fn start(
        &self,
        request: Request<StartRequest>,
    ) -> Result<Response<StartResponse>, Status> {
        debug!("Starting container");
        let request = request.into_inner();
        let mut container_guard = self.container.lock().await;
        if container_guard.is_none() || container_guard.as_ref().unwrap().id() != request.id {
            return Err(Status::new(tonic::Code::NotFound, "Container not found"));
        }
        let container = container_guard.as_mut().unwrap();
        if let Err(err) = container.start(&self.runtime).await {
            return Err(Status::new(
                tonic::Code::Internal,
                format!("Failed to start container: {}", err),
            ));
        }
        let pid = container.pid().unwrap();
        let raw_pid = pid.as_raw() as u32;
        Ok(Response::new(StartResponse { pid: raw_pid }))
    }

    async fn delete(
        &self,
        request: Request<DeleteRequest>,
    ) -> Result<Response<DeleteResponse>, Status> {
        debug!("Deleting container");
        let request = request.into_inner();
        let mut container_guard = self.container.lock().await;
        if container_guard.is_none() || container_guard.as_ref().unwrap().id() != request.id {
            return Err(Status::new(tonic::Code::NotFound, "Container not found"));
        }
        let container = container_guard.as_mut().unwrap();
        if let Err(err) = container.delete(&self.runtime).await {
            return Err(Status::new(
                tonic::Code::Internal,
                format!("Failed to delete container: {}", err),
            ));
        }
        let raw_pid = container.pid().unwrap().as_raw() as u32;
        *container_guard = None;
        Ok(Response::new(DeleteResponse { pid: raw_pid }))
    }

    async fn wait(&self, request: Request<WaitRequest>) -> Result<Response<WaitResponse>, Status> {
        debug!("Waiting for container");
        let request = request.into_inner();
        let container_guard = self.container.lock().await;
        if container_guard.is_none() || container_guard.as_ref().unwrap().id() != request.id {
            return Err(Status::new(tonic::Code::NotFound, "Container not found"));
        }
        drop(container_guard);
        let mut wait_channels = self.wait_channels.lock().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        wait_channels.push(tx);
        drop(wait_channels);
        let exit_status = match rx.recv().await {
            Some(exit_status) => exit_status,
            None => return Err(Status::new(tonic::Code::Aborted, "Container exited")),
        };
        Ok(Response::new(WaitResponse {
            exit_status: exit_status as u32,
        }))
    }

    async fn shutdown(&self, request: Request<ShutdownRequest>) -> Result<Response<()>, Status> {
        debug!("Shutting down container");
        let request = request.into_inner();
        let container_guard = self.container.lock().await;
        if container_guard.is_none() || container_guard.as_ref().unwrap().id() != request.id {
            return Err(Status::new(tonic::Code::NotFound, "Container not found"));
        }
        let pid = container_guard.as_ref().unwrap().pid().unwrap();
        // Kill the container so that all `TaskService::wait` calls will return.
        // This way, Tonic will respect the shutdown signal.
        if let Err(err) = kill(pid, Signal::SIGTERM) {
            error!("Failed to send SIGTERM to container: {}", err);
        }
        self.exit_signal.signal();
        Ok(Response::new(()))
    }
}
