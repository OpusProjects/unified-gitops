use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use unified_gitops::adapters::out::git::cli::CliGit;

#[tokio::main]
async fn main() {
    // Validate-only mode for CI on config repositories: load and validate the
    // configuration exactly as startup would, report, and exit — nothing
    // binds, no scheduler starts, and no secrets are resolved (a CI runner
    // validating YAML has no reason to hold the deployment's env vars).
    // Plain prints rather than tracing: this output is for a human or a CI
    // log, not for a log pipeline. The directory is the word after the flag,
    // unless that is another flag; else CONFIG_DIR, else ./config. Reading
    // only CONFIG_DIR made `--check-config ./myconf` check ./config and say
    // OK about a directory it never read.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(i) = args.iter().position(|arg| arg == "--check-config") {
        let config_dir = args
            .get(i + 1)
            .filter(|next| !next.starts_with("--"))
            .cloned()
            .or_else(|| std::env::var("CONFIG_DIR").ok())
            .unwrap_or_else(|| "config".to_string());
        match unified_gitops::config::load_config(&config_dir) {
            Ok(cfg) => {
                // Deprecated forms pass (they still load), but a config
                // repository's CI is where they are worth reading.
                for notice in cfg.deprecations() {
                    println!("deprecated: {notice}");
                }
                println!(
                    "configuration OK ({}): {} credentials, {} projects, {} webhooks",
                    config_dir,
                    cfg.credentials.len(),
                    cfg.projects.len(),
                    cfg.webhooks.len(),
                );
                return;
            }
            Err(e) => {
                eprintln!("configuration INVALID ({}): {}", config_dir, e);
                std::process::exit(1);
            }
        }
    }

    // Structured logging — level configurable with RUST_LOG env var
    // RUST_LOG=debug cargo run → shows debug+info+warn+error
    // RUST_LOG=unified_gitops=debug → only debug from our crate
    // Without RUST_LOG → default is info
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config_dir = std::env::var("CONFIG_DIR").unwrap_or_else(|_| "config".to_string());
    let cfg = match unified_gitops::config::load_config(&config_dir) {
        Ok(cfg) => cfg,
        Err(e) => {
            error!("Failed to load configuration: {}", e);
            std::process::exit(1);
        }
    };

    // Forms that still load but will be refused by a later release, named
    // with what to write instead.
    for notice in cfg.deprecations() {
        warn!("{notice}");
    }

    // What this process is about to be BUILT with, recorded before the
    // configuration is consumed. A later reload diffs against it to name the
    // keys it cannot adopt (see config::RestartOnlySettings).
    let live_settings = unified_gitops::config::RestartOnlySettings::from_config(&cfg);

    // Secrets are read here, at the boundary: the rest of the app receives
    // resolved keys as parameters and does not touch environment variables.
    let api_keys = match unified_gitops::adapters::r#in::http::auth::resolve_api_keys(&cfg) {
        Ok(keys) => keys,
        Err(e) => {
            error!("Failed to resolve API keys: {}", e);
            std::process::exit(1);
        }
    };

    if let Err(e) = unified_gitops::adapters::r#in::http::auth::ldap_requires_keys(&cfg, &api_keys)
    {
        error!("Refusing to start: {}", e);
        std::process::exit(1);
    }

    // Make an unauthenticated deployment loud, not a buried auth=false field
    if api_keys.is_empty() {
        warn!(
            "No API keys configured (api_keys.yaml or UNIFIED_GITOPS_KEY): \
             the /api/v1 API is running WITHOUT authentication"
        );
    }

    info!(
        webhooks = cfg.webhooks.len(),
        credentials = cfg.credentials.len(),
        projects = cfg.projects.len(),
        api_keys = api_keys.len(),
        "Configuration loaded"
    );

    // The chain, built by the library so that a configuration reload rebuilds
    // exactly the same one from the new credentials.
    let secrets: std::sync::Arc<dyn unified_gitops::ports::secrets::SecretsPort> =
        unified_gitops::adapters::out::secrets::build_chain(&cfg);

    // Created before the AppState exists because the boot clones below already
    // record into it; handed to the builder so the HTTP layer reads the same
    // instance those clones and the periodic project task write.
    let project_health =
        std::sync::Arc::new(unified_gitops::domain::sync_health::SyncHealthRegistry::new());

    // Writing the configuration directory over HTTP is opt-in: without it the
    // directory is read once at startup and never touched again, which is what
    // every deployment that has not asked for anything else expects.
    let config_store: Option<std::sync::Arc<unified_gitops::ports::config_store::ConfigStorePort>> =
        if cfg.config_api.enabled {
            info!(dir = %config_dir, "Configuration API enabled — this directory is writable over HTTP");
            Some(std::sync::Arc::new(
                unified_gitops::adapters::out::config::fs::fs_store(&config_dir),
            ))
        } else {
            None
        };

    let mut builder = unified_gitops::AppBuilder::new()
        .from_config(&cfg)
        .secrets(std::sync::Arc::clone(&secrets))
        .project_health(std::sync::Arc::clone(&project_health))
        .api_keys(api_keys);

    if let Some(store) = config_store {
        builder = builder.config_api(store, live_settings);
    }

    let (app, state) = builder.build_with_state();
    // Sends what runs have to tell (webhooks), from the first run on:
    // a failure of the very first check is worth knowing.
    state.start_webhooks();

    // One switch for every background task. Flipped after the HTTP server has
    // drained; each task returns at its next wait point (a running apply is
    // finished, never cut between operations) and main joins the handles
    // before exiting.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    // The handles to join at shutdown. Shared with the background boot task,
    // which is what starts the schedulers (after the clones) and therefore is
    // the one holding their handles.
    let background_tasks: std::sync::Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>> =
        std::sync::Arc::default();

    let addr = format!("{}:{}", cfg.server.host, cfg.server.port);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| {
            error!(addr = %addr, "Failed to bind: {}", e);
            std::process::exit(1);
        });

    info!(addr = %addr, "Listening");

    // The rest of the boot happens BEHIND the listener: project checkouts and
    // the sync schedulers must never gate /healthz. One unreachable git remote
    // used to mean the listener never bound at all — a failed startup probe
    // for a service whose HTTP layer was perfectly able to serve. Nothing
    // here depends on the checkouts existing before the router does.
    {
        let state = std::sync::Arc::clone(&state);
        let secrets = std::sync::Arc::clone(&secrets);
        let project_health = std::sync::Arc::clone(&project_health);
        let projects = cfg.projects.clone();
        let projects_dir = std::path::PathBuf::from(&cfg.projects_config.dir);
        let shutdown_rx = shutdown_rx.clone();
        let mut boot_shutdown = shutdown_rx.clone();
        let tasks = std::sync::Arc::clone(&background_tasks);

        let boot_handle = tokio::spawn(async move {
            if !projects.is_empty() {
                let git: std::sync::Arc<dyn unified_gitops::ports::git::GitPort> =
                    std::sync::Arc::new(CliGit::new());
                let venv: std::sync::Arc<dyn unified_gitops::ports::venv::VenvPort> =
                    std::sync::Arc::new(unified_gitops::adapters::out::python::PyVenv::new());

                // Concurrently rather than one after the other: boot waits for
                // the slowest clone, not the sum — and every clone is bounded
                // by its project's timeout_seconds (applied in sync_project).
                let mut clones = tokio::task::JoinSet::new();
                for (project_id, project) in projects.clone() {
                    // sync_on_boot=false + existing checkout (e.g. a persistent
                    // volume) = start offline from what is on disk; updates then
                    // come from the interval or POST /api/v1/projects/{id}/sync.
                    // A missing checkout is always cloned — no scripts, nothing
                    // to run.
                    let checkout_exists =
                        tokio::fs::try_exists(projects_dir.join(&project_id).join(".git"))
                            .await
                            .unwrap_or(false);
                    if !project.sync_on_boot && checkout_exists {
                        info!(project = %project_id, "Using existing checkout (sync_on_boot: false)");
                        continue;
                    }

                    let git = std::sync::Arc::clone(&git);
                    let secrets = std::sync::Arc::clone(&secrets);
                    let venv = std::sync::Arc::clone(&venv);
                    let project_health = std::sync::Arc::clone(&project_health);
                    let projects_dir = projects_dir.clone();
                    let state = std::sync::Arc::clone(&state);
                    clones.spawn(async move {
                        // A pull at boot tells what it brought too: commits
                        // pushed while this instance was down.
                        let config = state.config();
                        match unified_gitops::application::projects::sync_project(
                            &*git,
                            &*secrets,
                            &*venv,
                            &project_health,
                            &project_id,
                            &project,
                            &projects_dir,
                            Some(unified_gitops::application::projects::Announce {
                                dispatcher: &state.dispatcher,
                                webhooks: &config.webhooks,
                                url: config.url.as_deref(),
                            }),
                            unified_gitops::application::projects::RunOrigin::of(
                                unified_gitops::domain::sync_health::RunTrigger::Startup,
                            ),
                        )
                        .await
                        {
                            Ok(()) => info!(project = %project_id, "Project checkout ready"),
                            Err(e) => {
                                error!(project = %project_id, error = %e, "Project sync failed")
                            }
                        }
                    });
                }
                // A shutdown arriving mid-clone aborts the remaining clones
                // (dropping them kills the git children) instead of starting
                // schedulers nobody wants anymore.
                loop {
                    tokio::select! {
                        next = clones.join_next() => {
                            if next.is_none() {
                                break;
                            }
                        }
                        _ = boot_shutdown.changed() => {
                            clones.abort_all();
                            return;
                        }
                    }
                }
            }

            if *boot_shutdown.borrow() {
                return;
            }
            // After the boot clones had their chance (bounded by their
            // timeouts), so a project's first scheduled pull does not race
            // its own clone for no reason but start order.
            //
            // One supervisor rather than the task handles directly: it owns
            // every periodic task (project pulls) and
            // replaces the whole set when a configuration reload lands.
            let supervisor =
                unified_gitops::adapters::r#in::scheduler::start_supervisor(state, shutdown_rx);
            tasks.lock().expect("handle registry").push(supervisor);
        });
        background_tasks
            .lock()
            .expect("handle registry")
            .push(boot_handle);
    }

    // Graceful shutdown — waits for SIGTERM or Ctrl+C
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap_or_else(|e| {
            error!("Server error: {}", e);
            std::process::exit(1);
        });

    // Drain the background tasks: signal them, then wait — bounded by
    // shutdown_grace_seconds — for in-flight runs to finish. Past the grace
    // the process exits anyway: blocking on a wedged pull would only trade
    // a clean exit for a SIGKILL.
    let shutdown_started = std::time::Instant::now();
    let _ = shutdown_tx.send(true);
    let handles: Vec<tokio::task::JoinHandle<()>> =
        std::mem::take(background_tasks.lock().expect("handle registry").as_mut());
    // From the CURRENT snapshot, not the boot config: the grace is read at
    // this single moment, which is what makes it reloadable — whatever a
    // reload set last is what governs this drain.
    let grace_seconds = state.config().shutdown_grace_seconds;
    let grace = std::time::Duration::from_secs(grace_seconds);
    let drain = async {
        for handle in handles {
            let _ = handle.await;
        }
    };
    if tokio::time::timeout(grace, drain).await.is_err() {
        warn!(
            grace_seconds,
            "Background tasks still running after the grace period — exiting anyway"
        );
    } else {
        info!("Background tasks drained");
    }

    // Then the webhooks those last runs queued, in what is left of the grace:
    // a drift reverted in the very last run must still be reported.
    let left = state
        .dispatcher
        .drain(grace.saturating_sub(shutdown_started.elapsed()))
        .await;
    if left > 0 {
        warn!(left, "Webhook messages still unsent at shutdown — dropped");
    }

    info!("Shutdown complete");
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("Received Ctrl+C, shutting down"),
        _ = terminate => info!("Received SIGTERM, shutting down"),
    }
}
