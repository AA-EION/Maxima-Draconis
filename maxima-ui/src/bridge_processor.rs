use crate::{
    bridge_thread::{self, BackendError},
    views::downloads_view::QueuedDownload,
    BackendStallState, GameDetails, GameDetailsWrapper, MaximaEguiApp, PageType,
};
use log::{error, info, warn};
use std::sync::mpsc::TryRecvError;

pub fn frontend_processor(app: &mut MaximaEguiApp, ctx: &egui::Context) {
    puffin::profile_function!();

    if app.critical_error.is_some() {
        return;
    }

    'outer: loop {
        match app.backend.backend_listener.try_recv() {
            Ok(result) => {
                use bridge_thread::MaximaLibResponse::*;
                match result {
                    LoginResponse(res) => {
                        if let Err(error) = &res {
                            warn!("Login failed. {}", error);
                            continue;
                        }
                        let res = res.unwrap();

                        info!("Logged in as {}!", &res.name);
                        app.user_name = res.name;
                        app.user_id = res.id;
                        app.backend_state = BackendStallState::BingChilling;
                        app.backend
                            .backend_commander
                            .send(bridge_thread::MaximaLibRequest::GetGamesRequest)
                            .unwrap();
                        app.backend
                            .backend_commander
                            .send(bridge_thread::MaximaLibRequest::GetFriendsRequest)
                            .unwrap();

                        // External-command auto-install: if the user
                        // (or an external launcher)
                        // passed `--install <slug>` on the command
                        // line, fire it now that the login has
                        // landed. `take()` so we never re-fire on a
                        // re-login event.
                        if let Some((slug, path)) = app.pending_install.take() {
                            info!(
                                "Dispatching auto-install for '{}' -> {:?}",
                                slug, path
                            );
                            // Tolerate a dead bridge thread: an
                            // `.unwrap()` here would panic the UI
                            // thread on a `SendError`, which is the
                            // exact failure mode the new panic hook
                            // is meant to help diagnose — don't
                            // compound it. The next try_recv on the
                            // listener will surface a Disconnected
                            // error and route to `critical_error`,
                            // which is the right user-visible
                            // outcome.
                            if let Err(err) = app.backend.backend_commander.send(
                                bridge_thread::MaximaLibRequest::AutoInstallSlug(slug, path),
                            ) {
                                warn!(
                                    "Failed to dispatch AutoInstallSlug — bridge thread \
                                     disconnected? {}",
                                    err
                                );
                            }
                            // Jump to the Downloads view so the user
                            // sees progress as soon as the queue
                            // update arrives — saves them clicking
                            // a tab they didn't ask to land on.
                            app.page_view = PageType::Downloads;
                        }
                    }
                    LoginCacheEmpty => {
                        app.backend_state = BackendStallState::UserNeedsToLogIn;
                        // `--install` (Draconis onboarding) logs in without a click, once.
                        if app.pending_install.is_some() && !app.auto_login_sent {
                            app.auto_login_sent = true;
                            let _ = app
                                .backend
                                .backend_commander
                                .send(bridge_thread::MaximaLibRequest::LoginRequestOauth);
                            app.backend_state = BackendStallState::LoggingIn;
                        }
                    }
                    ServiceNeedsStarting => {
                        app.backend_state = BackendStallState::UserNeedsToInstallService
                    }
                    ServiceStarted => app.backend_state = BackendStallState::Starting,
                    GameInfoResponse(res) => {
                        app.games.insert(res.game.slug.clone(), res.game);
                    }
                    #[cfg(feature = "bg-videos")]
                    GameBgVideoResponse(slug, url) => app.bg_video.set_url(slug, url),
                    GameDetailsResponse(res) => {
                        let response = res.response;

                        for (slug, game) in &mut app.games {
                            if !slug.eq(&res.slug) {
                                continue;
                            }

                            game.details = GameDetailsWrapper::Available(GameDetails {
                                time: response.time,
                                achievements_unlocked: response.achievements_unlocked,
                                achievements_total: response.achievements_total,
                                path: response.path.clone(),
                                system_requirements_min: response.system_requirements_min.clone(),
                                system_requirements_rec: response.system_requirements_rec.clone(),
                            });
                        }
                    }
                    FriendInfoResponse(res) => app.friends.push(res.friend),
                    CriticalError(err) => app.critical_error = Some(*err),
                    NonFatalError(err) => app.nonfatal_errors.push(*err),
                    ActiveGameChanged(slug) => app.playing_game = slug,
                    LocateGameResponse(res) => {
                        app.installer_state.locate_response = Some(res);
                        app.installer_state.locating = false;
                    }
                    DownloadProgressChanged(slug, progress) => {
                        if let Some(dl_ing) = app.installing_now.as_mut().filter(|d| d.slug == slug) {
                            dl_ing.percent = progress.percent;
                            dl_ing.downloaded_bytes = progress.bytes;
                            dl_ing.total_bytes = progress.bytes_total;
                        }
                    }
                    DownloadFinished(_) => {}
                    DownloadQueueUpdate(current, queue, paused) => {
                        let queued = |slug: String| QueuedDownload {
                            offer: app.games.get(&slug).map(|g| g.offer.clone()).unwrap_or_default(),
                            slug,
                            percent: 0.0,
                            downloaded_bytes: 0,
                            total_bytes: 0,
                        };
                        let now = match current {
                            Some(slug) if app.installing_now.as_ref().is_some_and(|n| n.slug == slug) => {
                                app.installing_now.take()
                            }
                            Some(slug) => Some(queued(slug)),
                            None => None,
                        };
                        app.install_queue = queue.into_iter().map(queued).collect();
                        app.installing_now = now;
                        app.downloads_paused = paused;
                    }
                }
                ctx.request_repaint();
            }
            Err(variant) => {
                match variant {
                    TryRecvError::Empty => {}
                    TryRecvError::Disconnected => {
                        app.critical_error = Some(BackendError::ChannelDisconnected);
                    }
                }
                break 'outer;
            }
        }
    }
}
