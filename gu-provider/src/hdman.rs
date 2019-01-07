use super::{
    envman, status,
    sync_exec::{Exec, ExecResult, SyncExecManager},
};
use actix::{fut, prelude::*};
use actix_web::client;
use actix_web::error::ErrorInternalServerError;
use deployment::DeployManager;
use deployment::Destroy;
use deployment::IntoDeployInfo;
use futures::prelude::*;
use gu_actix::prelude::*;
use gu_base::files::read_async;
use gu_model::envman::*;
use gu_net::rpc::{
    peer::{PeerSessionInfo, PeerSessionStatus},
    *,
};
use gu_persist::config::ConfigModule;
use id::generate_new_id;
use provision::{download, untgz};
use std::{collections::HashMap, fs, path::PathBuf, process, result, time};
use workspace::Workspace;

impl IntoDeployInfo for SessionInfo {
    fn convert(&self, id: &String) -> PeerSessionInfo {
        PeerSessionInfo {
            id: id.clone(),
            name: self.workspace.get_name().clone(),
            status: self.status.clone(),
            tags: self.workspace.get_tags(),
            note: self.note.clone(),
            processes: self.processes.keys().cloned().collect(),
        }
    }
}

impl Destroy for SessionInfo {
    fn destroy(&mut self) -> Result<(), Error> {
        debug!("killing all running child processes");
        let _ = self
            .processes
            .values_mut()
            .map(|child| child.kill())
            .collect::<Vec<_>>();
        let _ = self
            .processes
            .values_mut()
            .map(|child| child.wait())
            .collect::<Vec<_>>();
        self.workspace.clear_dir().map_err(From::from)
    }
}

/// Host direct manager
pub struct HdMan {
    deploys: DeployManager<SessionInfo>,
    cache_dir: PathBuf,
    sessions_dir: PathBuf,
}

impl envman::EnvManService for HdMan {
    type CreateOptions = ();
}

impl Actor for HdMan {
    type Context = RemotingContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        envman::register("hd", ctx.address());

        status::StatusManager::from_registry().do_send(status::AddProvider::new(
            "hostDirect",
            ctx.address().recipient(),
        ));

        ctx.run_interval(time::Duration::from_secs(10), |act, _| {
            act.scan_for_processes()
        });
    }
}

impl HdMan {
    pub fn start(config: &ConfigModule) -> Addr<Self> {
        let cache_dir = config.cache_dir().to_path_buf().join("images");
        let sessions_dir = config.work_dir().to_path_buf().join("sessions");

        debug!(
            "creating dirs for:\nimage cache {:?}\nsessions:{:?}",
            cache_dir, sessions_dir
        );
        fs::create_dir_all(&cache_dir)
            .and_then(|_| fs::create_dir_all(&sessions_dir))
            .map_err(|e| error!("Cannot create HdMan dir: {:?}", e))
            .unwrap();

        start_actor(HdMan {
            deploys: Default::default(),
            cache_dir,
            sessions_dir,
        })
    }

    fn get_session_path(&self, session_id: &String) -> PathBuf {
        self.sessions_dir.join(session_id)
    }

    fn get_cache_path(&self, file_name: &String) -> PathBuf {
        self.cache_dir.join(file_name)
    }

    fn get_session_exec_path(&self, session_id: &String, executable: &String) -> String {
        self.get_session_path(session_id)
            .join(executable.trim_left_matches('/'))
            .into_os_string()
            .into_string()
            .unwrap()
    }

    fn get_session_mut(&mut self, session_id: &String) -> Result<&mut SessionInfo, Error> {
        match self.deploys.deploy_mut(session_id) {
            Ok(session) => Ok(session),
            Err(_) => Err(Error::NoSuchSession(session_id.clone())),
        }
    }

    fn insert_child(
        &mut self,
        session_id: &String,
        child: process::Child,
    ) -> Result<String, Error> {
        Ok(self.get_session_mut(&session_id)?.insert_process(child))
    }

    fn scan_for_processes(&mut self) {
        for sess_info in self.deploys.values_mut() {
            let finished: Vec<String> = sess_info
                .processes
                .iter_mut()
                .filter_map(|(id, child)| match child.try_wait() {
                    Ok(Some(_exit_st)) => Some(id.clone()),
                    _ => None,
                })
                .collect();

            let some_finished = !finished.is_empty();
            for f in finished {
                sess_info.processes.remove(&f);
                info!("finished {:?}; removing", f)
            }

            if some_finished & sess_info.processes.is_empty() {
                sess_info.status = PeerSessionStatus::CONFIGURED;
            }
        }
    }
}

/// internal session representation
struct SessionInfo {
    workspace: Workspace,
    status: PeerSessionStatus,
    /// used to determine proper status when last child is finished
    dirty: bool,
    note: Option<String>,
    processes: HashMap<String, process::Child>,
}

impl SessionInfo {
    fn insert_process(&mut self, child: process::Child) -> String {
        let id = generate_new_id(&self.processes);
        self.processes.insert(id.clone(), child);
        self.dirty = true;
        self.status = PeerSessionStatus::RUNNING;
        id
    }
}

impl Handler<CreateSession> for HdMan {
    type Result = ActorResponse<HdMan, String, Error>;

    fn handle(
        &mut self,
        msg: CreateSession,
        _ctx: &mut Self::Context,
    ) -> <Self as Handler<CreateSession>>::Result {
        let session_id = self.deploys.generate_session_id();
        let work_dir = self.get_session_path(&session_id);

        let cache_path = self.get_cache_path(&msg.image.hash);
        let mut workspace = Workspace::new(msg.name, work_dir.clone());
        workspace.add_tags(msg.tags);

        match workspace.create_dirs() {
            Ok(_) => (),
            Err(e) => return ActorResponse::reply(Err(e.into())),
        }

        let session = SessionInfo {
            workspace,
            status: PeerSessionStatus::PENDING,
            dirty: false,
            note: msg.note,
            processes: HashMap::new(),
        };

        debug!("newly created session id={}", session_id);
        self.deploys.insert_deploy(session_id.clone(), session);

        debug!("hey! I'm downloading from: {:?}", msg.image);
        let sess_id = session_id.clone();
        ActorResponse::async(
            download(msg.image.url.as_ref(), cache_path.clone(), true)
                .map_err(From::from)
                .and_then(move |_| untgz(cache_path, work_dir))
                .map_err(From::from)
                .into_actor(self)
                .and_then(|_, act, _ctx| match act.get_session_mut(&sess_id) {
                    Ok(session) => {
                        session.status = PeerSessionStatus::CREATED;
                        fut::ok(sess_id)
                    }
                    Err(e) => fut::err(e),
                })
                .map_err(
                    move |e, act, _ctx| match act.deploys.destroy_deploy(&session_id) {
                        Ok(_) => Error::IoError(format!("creating session error: {:?}", e)),
                        Err(e) => e,
                    },
                ),
        )
    }
}

impl Handler<SessionUpdate> for HdMan {
    /// ok: succeeded cmds output
    /// err: all succeeded cmds output till first failure, plus failed cmd err msg
    type Result = ActorResponse<HdMan, Vec<String>, Vec<String>>;

    fn handle(&mut self, msg: SessionUpdate, _ctx: &mut Self::Context) -> Self::Result {
        if !self.deploys.contains_deploy(&msg.session_id) {
            return ActorResponse::reply(Err(
                vec![Error::NoSuchSession(msg.session_id).to_string()],
            ));
        }

        let mut future_chain: Box<
            ActorFuture<Item = Vec<String>, Error = Vec<String>, Actor = Self>,
        > = Box::new(fut::ok(Vec::new()));

        for cmd in msg.commands {
            let session_id = msg.session_id.clone();
            let session_dir = self.get_session_path(&session_id).to_owned();

            match cmd {
                Command::Open => (),
                Command::Close => (),
                Command::Exec { executable, args } => {
                    let executable = self.get_session_exec_path(&session_id, &executable);
                    future_chain = Box::new(future_chain.and_then(move |mut v, act, _ctx| {
                        let mut vc = v.clone();
                        info!("executing sync: {} {:?}", executable, args);
                        SyncExecManager::from_registry()
                            .send(Exec::Run {
                                executable,
                                args,
                                cwd: session_dir,
                            })
                            .flatten_fut()
                            .map_err(|e| {
                                vc.push(e.to_string());
                                vc
                            })
                            .into_actor(act)
                            .and_then(move |result, act, _ctx| {
                                info!("sync cmd result: {:?}", result);
                                if let ExecResult::Run(output) = result {
                                    v.push(String::from_utf8_lossy(&output.stdout).to_string());
                                }
                                match act.get_session_mut(&session_id) {
                                    Ok(session) => {
                                        session.dirty = true;
                                        fut::ok(v)
                                    }
                                    Err(e) => {
                                        v.push(e.to_string());
                                        fut::err(v)
                                    }
                                }
                            })
                    }));
                }
                Command::Start { executable, args } => {
                    let executable = self.get_session_exec_path(&session_id, &executable);
                    future_chain = Box::new(future_chain.and_then(move |mut v, act, _ctx| {
                        info!("executing async: {} {:?}", executable, args);
                        // TODO: critical section
                        // TODO: env::set_current_dir(&base_dir)?;
                        let mut vc = v.clone();
                        process::Command::new(&executable)
                            .args(&args)
                            .spawn()
                            .map_err(|e| Error::IoError(e.to_string()))
                            .and_then(|child| act.insert_child(&session_id, child))
                            .and_then(|child_id| {
                                v.push(child_id);
                                Ok(fut::ok(v))
                            })
                            .or_else(|e| {
                                vc.push(e.to_string());
                                Ok(fut::err(vc))
                            })
                            .map_err(|e: Error| e)
                            .unwrap()
                    }));
                }
                Command::Stop { child_id } => {
                    future_chain = Box::new(future_chain.and_then(move |mut v, act, _ctx| {
                        let mut vc = v.clone();
                        info!("killing: {:?}", &child_id);
                        match act.get_session_mut(&session_id) {
                            Ok(session) => match session.processes.remove(&child_id) {
                                Some(child) => fut::Either::A(
                                    fut::wrap_future(
                                        SyncExecManager::from_registry().send(Exec::Kill(child)),
                                    )
                                    .map_err(|e, _act: &mut Self, _ctx| {
                                        vc.push(format!("{}", e));
                                        vc
                                    })
                                    .and_then(
                                        move |result, act, _ctx| {
                                            if let Ok(ExecResult::Kill(output)) = result {
                                                match act.get_session_mut(&session_id) {
                                                    Ok(mut session) => {
                                                        if session.processes.is_empty() {
                                                            session.status =
                                                                PeerSessionStatus::CONFIGURED;
                                                        };
                                                        v.push(output);
                                                        fut::ok(v)
                                                    }
                                                    Err(e) => {
                                                        v.push(e.to_string());
                                                        fut::err(v)
                                                    }
                                                }
                                            } else {
                                                v.push(format!("wrong result {:?}", result));
                                                fut::err(v)
                                            }
                                        },
                                    ),
                                ),
                                None => {
                                    v.push(Error::NoSuchChild(child_id).to_string());
                                    fut::Either::B(fut::err(v))
                                }
                            },
                            Err(e) => {
                                v.push(e.to_string());
                                fut::Either::B(fut::err(v))
                            }
                        }
                    }));
                }
                Command::AddTags(mut tags) => {
                    future_chain = Box::new(future_chain.and_then(move |mut v, act, _ctx| {
                        match act.get_session_mut(&session_id) {
                            Ok(session) => {
                                tags.into_iter().for_each(|tag| {
                                    session.workspace.add_tags(vec![tag]);
                                });
                                v.push(format!(
                                    "tags inserted. Current tags are: {:?}",
                                    &session.workspace.get_tags()
                                ));
                                fut::ok(v)
                            }
                            Err(e) => {
                                v.push(e.to_string());
                                fut::err(v)
                            }
                        }
                    }));
                }
                Command::DelTags(mut tags) => {
                    future_chain = Box::new(future_chain.and_then(move |mut v, act, _ctx| {
                        match act.get_session_mut(&session_id) {
                            Ok(session) => {
                                session.workspace.remove_tags(tags);
                                v.push(format!(
                                    "tags removed. Current tags are: {:?}",
                                    &session.workspace.get_tags()
                                ));
                                fut::ok(v)
                            }
                            Err(e) => {
                                v.push(e.to_string());
                                fut::err(v)
                            }
                        }
                    }));
                }
                Command::DownloadFile {
                    uri,
                    file_path,
                    format,
                } => {
                    let path = self.get_session_path(&session_id).join(file_path);
                    future_chain = Box::new(handle_download_file(future_chain, uri, path, format));
                }
                Command::UploadFile {
                    uri,
                    file_path,
                    format,
                } => {
                    let path = self.get_session_path(&session_id).join(file_path);
                    future_chain = Box::new(handle_upload_file(future_chain, uri, path, format));
                }
            }
        }
        ActorResponse::async(future_chain)
    }
}

fn handle_download_file(
    future_chain: Box<ActorFuture<Item = Vec<String>, Error = Vec<String>, Actor = HdMan>>,
    uri: String,
    file_path: PathBuf,
    _format: ResourceFormat,
) -> impl ActorFuture<Item = Vec<String>, Error = Vec<String>, Actor = HdMan> {
    future_chain.and_then(move |mut v, act, _ctx| {
        download(uri.as_ref(), file_path, false)
            .then(move |x| match x {
                Ok(()) => {
                    v.push(format!("{:?} file downloaded", uri));
                    Ok(v)
                }
                Err(e) => {
                    v.push(e.to_string());
                    Err(v)
                }
            })
            .into_actor(act)
    })
}

fn handle_upload_file(
    future_chain: Box<ActorFuture<Item = Vec<String>, Error = Vec<String>, Actor = HdMan>>,
    uri: String,
    file_path: PathBuf,
    _format: ResourceFormat,
) -> impl ActorFuture<Item = Vec<String>, Error = Vec<String>, Actor = HdMan> {
    future_chain.and_then(move |mut v, act, _ctx| {
        match client::put(uri.clone())
            .streaming(read_async(file_path).map_err(|e| ErrorInternalServerError(e)))
        {
            Ok(req) => fut::Either::A(
                req.send()
                    .map_err(|e| e.to_string())
                    .then(move |x| {
                        x.and_then(|res| {
                            if res.status().is_success() {
                                v.push(format!("{:?} file uploaded", uri));
                                Ok(v.clone())
                            } else {
                                Err(format!("Unsuccessful file upload: {}", res.status()))
                            }
                        })
                        .map_err(|e| {
                            v.push(e.to_string());
                            v
                        })
                    })
                    .into_actor(act),
            ),
            Err(e) => {
                v.push(e.to_string());
                fut::Either::B(fut::err(v))
            }
        }
    })
}

// TODO: implement child process polling and status reporting
#[derive(Serialize, Deserialize, Debug)]
struct SessionStatus {
    session_id: String,
}

impl Handler<GetSessions> for HdMan {
    type Result = result::Result<Vec<PeerSessionInfo>, ()>;

    fn handle(&mut self, _msg: GetSessions, _ctx: &mut Self::Context) -> Self::Result {
        Ok(self.deploys.deploys_info())
    }
}

impl Handler<DestroySession> for HdMan {
    type Result = ActorResponse<HdMan, String, Error>;

    fn handle(
        &mut self,
        msg: DestroySession,
        _ctx: &mut Self::Context,
    ) -> <Self as Handler<DestroySession>>::Result {
        ActorResponse::async(match self.deploys.destroy_deploy(&msg.session_id) {
            Ok(_) => fut::ok("Session closed".into()),
            Err(e) => fut::err(e),
        })
    }
}

impl Handler<status::GetEnvStatus> for HdMan {
    type Result = MessageResult<status::GetEnvStatus>;

    fn handle(
        &mut self,
        _msg: status::GetEnvStatus,
        _ctx: &mut Self::Context,
    ) -> <Self as Handler<status::GetEnvStatus>>::Result {
        MessageResult(self.deploys.status())
    }
}
