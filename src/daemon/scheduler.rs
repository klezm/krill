//! Deal with asynchronous scheduled processes, either triggered by an
//! event that occurred, or planned (e.g. re-publishing).

use std::sync::Arc;
use std::time::Duration;

use clokwerk::{self, ScheduleHandle, TimeUnits};
use tokio::runtime::Runtime;

use rpki::x509::Time;

use crate::commons::{actor::Actor, api::Handle};
use crate::commons::bgp::BgpAnalyser;
#[cfg(feature = "multi-user")]
use crate::daemon::auth::common::session::LoginSessionCache;
use crate::daemon::ca::CaServer;
use crate::daemon::config::Config;
use crate::daemon::mq::{EventQueueListener, QueueEvent};
use crate::pubd::PubServer;
use crate::publish::CaPublisher;

pub struct Scheduler {
    /// Responsible for listening to events and executing triggered processes, such
    /// as publication of newly generated RPKI objects.
    #[allow(dead_code)] // just need to keep this in scope
    event_sh: ScheduleHandle,

    /// Responsible for periodically republishing so that MFTs and CRLs do not go stale.
    #[allow(dead_code)] // just need to keep this in scope
    republish_sh: ScheduleHandle,

    /// Responsible for letting CA check with their parents whether their resource
    /// entitlements have changed *and* for the shrinking of issued certificates, if
    /// they are not renewed within the configured grace period.
    #[allow(dead_code)] // just need to keep this in scope
    ca_refresh_sh: ScheduleHandle,

    /// Responsible for refreshing announcement information
    #[allow(dead_code)] // just need to keep this in scope
    announcements_refresh_sh: ScheduleHandle,

    /// Responsible for archiving old commands
    #[allow(dead_code)] // just need to keep this in scope
    archive_old_commands_sh: ScheduleHandle,

    #[cfg(feature = "multi-user")]
    /// Responsible for purging expired cached login tokens
    #[allow(dead_code)] // just need to keep this in scope
    login_cache_sweeper_sh: ScheduleHandle,
}

impl Scheduler {
    pub fn build(
        event_queue: Arc<EventQueueListener>,
        caserver: Arc<CaServer>,
        pubserver: Option<Arc<PubServer>>,
        bgp_analyser: Arc<BgpAnalyser>,
        #[cfg(feature = "multi-user")]
        login_session_cache: Arc<LoginSessionCache>,
        config: &Config,
        actor: &Actor,
    ) -> Self {
        let event_sh = make_event_sh(event_queue, caserver.clone(), pubserver.clone(), config.test_mode, actor.clone());
        let republish_sh = make_republish_sh(caserver.clone(), actor.clone());
        let ca_refresh_sh = make_ca_refresh_sh(caserver.clone(), config.ca_refresh, actor.clone());
        let announcements_refresh_sh = make_announcements_refresh_sh(bgp_analyser);
        let archive_old_commands_sh = make_archive_old_commands_sh(caserver, pubserver, config.archive_threshold_days, actor.clone());
        #[cfg(feature = "multi-user")]
        let login_cache_sweeper_sh = make_login_cache_sweeper_sh(login_session_cache);
        Scheduler {
            event_sh,
            republish_sh,
            ca_refresh_sh,
            announcements_refresh_sh,
            archive_old_commands_sh,
            #[cfg(feature = "multi-user")]
            login_cache_sweeper_sh,
        }
    }
}

#[allow(clippy::cognitive_complexity)]
fn make_event_sh(
    event_queue: Arc<EventQueueListener>,
    caserver: Arc<CaServer>,
    pubserver: Option<Arc<PubServer>>,
    test_mode: bool,
    actor: Actor,
) -> ScheduleHandle {
    let mut scheduler = clokwerk::Scheduler::new();
    scheduler.every(1.seconds()).run(move || {
        let mut rt = Runtime::new().unwrap();

        rt.block_on( async {
            for evt in event_queue.pop_all() {
                match evt {
                    QueueEvent::ServerStarted => {
                        info!("Will re-sync all CAs with their parents and repository after startup");
                        caserver.resync_all(&actor).await;
                        let publisher = CaPublisher::new(caserver.clone(), pubserver.clone());
                        match caserver.ca_list(&actor) {
                            Err(e) => error!("Unable to obtain CA list: {}", e),
                            Ok(list) => {
                                for ca in list.cas() {
                                    if publisher.publish(ca.handle(), &actor).await.is_err() {
                                        error!("Unable to synchronise CA '{}' with its repository after startup", ca.handle());
                                    } else {
                                        info!("CA '{}' is in sync with its repository", ca.handle());
                                    }
                                }
                            }
                        }
                    }

                    QueueEvent::Delta(handle, _version) => {
                        try_publish(&event_queue, caserver.clone(), pubserver.clone(), handle, test_mode, &actor).await
                    }
                    QueueEvent::ReschedulePublish(handle, last_try) => {
                        if Time::five_minutes_ago().timestamp() > last_try.timestamp() {
                            try_publish(&event_queue, caserver.clone(), pubserver.clone(), handle, test_mode, &actor).await
                        } else {
                            event_queue.push_back(QueueEvent::ReschedulePublish(handle, last_try));
                        }
                    }
                    QueueEvent::ResourceClassRemoved(handle, _, parent, revocations) => {
                        info!("Trigger send revoke requests for removed RC for '{}' under '{}'",handle,parent);

                        if caserver.send_revoke_requests(&handle, &parent, revocations, &actor).await.is_err() {
                            warn!("Could not revoke key for removed resource class. This is not \
                            an issue, because typically the parent will revoke our keys pro-actively, \
                            just before removing the resource class entitlements.");
                        }
                    }
                    QueueEvent::UnexpectedKey(handle, _, rcn, revocation) => {
                            info!(
                                "Trigger sending revocation requests for unexpected key with id '{}' in RC '{}'",
                                revocation.key(),
                                rcn
                            );
                            if let Err(e) = caserver
                                .send_revoke_unexpected_key(&handle, rcn, revocation, &actor).await {
                                error!("Could not revoke unexpected surplus key at parent: {}", e);
                            }
                    }
                    QueueEvent::ParentAdded(handle, _, parent) => {
                            info!(
                                "Get updates for '{}' from added parent '{}'.",
                                handle,
                                parent
                            );
                            if let Err(e) = caserver.get_updates_from_parent(&handle, &parent, &actor).await {
                                error!(
                                    "Error getting updates for '{}', from parent '{}',  error: '{}'",
                                    &handle, &parent, e
                                )
                            }
                    }
                    QueueEvent::RepositoryConfigured(ca, _) => {
                            info!("Repository configured for '{}'", ca);
                            if let Err(e) = caserver.get_delayed_updates(&ca, &actor).await {
                                error!(
                                    "Error getting updates after configuring repository for '{}',  error: '{}'",
                                    &ca, e
                                )
                            }
                    }

                    QueueEvent::RequestsPending(handle, _) => {
                            info!("Get updates for pending requests for '{}'.", handle);
                            if let Err(e) = caserver.send_all_requests(&handle, &actor).await {
                                error!(
                                    "Failed to send pending requests for '{}', error '{}'",
                                    &handle, e
                                );
                            }
                    }
                    QueueEvent::CleanOldRepo(handle, _) => {
                            let publisher = CaPublisher::new(caserver.clone(), pubserver.clone());
                            if let Err(e) = publisher.clean_up(&handle, &actor).await {
                                info!(
                                    "Could not clean up old repo for '{}', it may be that it's no longer available. Got error '{}'",
                                    &handle, e
                                );
                            }
                            if let Err(e) = caserver.remove_old_repo(&handle, &actor).await {
                                error!(
                                    "Failed to remove old repo from ca '{}', error '{}'",
                                    &handle, e
                                );
                            }
                    }
                }
            }
        });


    });
    scheduler.watch_thread(Duration::from_millis(100))
}

async fn try_publish(
    event_queue: &Arc<EventQueueListener>,
    caserver: Arc<CaServer>,
    pubserver: Option<Arc<PubServer>>,
    ca: Handle,
    test_mode: bool,
    actor: &Actor,
) {
    info!("Try to publish for '{}'", ca);
    let publisher = CaPublisher::new(caserver.clone(), pubserver);

    if let Err(e) = publisher.publish(&ca, actor).await {
        if test_mode {
            error!("Failed to publish for '{}', error: {}", ca, e);
        } else {
            error!("Failed to publish for '{}' will reschedule, error: {}", ca, e);
            event_queue.push_back(QueueEvent::ReschedulePublish(ca, Time::now()));
        }
    }
}

fn make_republish_sh(caserver: Arc<CaServer>, actor: Actor) -> ScheduleHandle {
    let mut scheduler = clokwerk::Scheduler::new();
    scheduler.every(2.minutes()).run(move || {
        let mut rt = Runtime::new().unwrap();
        rt.block_on(async {
            info!("Triggering background republication for all CAs");
            if let Err(e) = caserver.republish_all(&actor).await {
                error!("Background republishing failed: {}", e);
            }
        })
    });
    scheduler.watch_thread(Duration::from_millis(100))
}

fn make_ca_refresh_sh(caserver: Arc<CaServer>, refresh_rate: u32, actor: Actor) -> ScheduleHandle {
    let mut scheduler = clokwerk::Scheduler::new();
    scheduler.every(refresh_rate.seconds()).run(move || {
        let mut rt = Runtime::new().unwrap();
        rt.block_on(async {
            info!("Triggering background refresh for all CAs");
            caserver.resync_all(&actor).await
        })
    });
    scheduler.watch_thread(Duration::from_millis(100))
}

fn make_announcements_refresh_sh(bgp_analyser: Arc<BgpAnalyser>) -> ScheduleHandle {
    let mut scheduler = clokwerk::Scheduler::new();
    scheduler.every(1.seconds()).run(move || {
        let mut rt = Runtime::new().unwrap();
        rt.block_on(async {
            if let Err(e) = bgp_analyser.update().await {
                error!("Failed to update BGP announcements: {}", e)
            }
        })
    });
    scheduler.watch_thread(Duration::from_millis(100))
}

fn make_archive_old_commands_sh(
    caserver: Arc<CaServer>,
    pubserver: Option<Arc<PubServer>>,
    archive_threshold_days: Option<i64>,
    actor: Actor,
) -> ScheduleHandle {
    let mut scheduler = clokwerk::Scheduler::new();
    scheduler.every(1.hours()).run(move || {
        let mut rt = Runtime::new().unwrap();
        rt.block_on(async {
            if let Some(days) = archive_threshold_days {
                if let Err(e) = caserver.archive_old_commands(days, &actor).await {
                    error!("Failed to archive old CA commands: {}", e)
                }

                if let Some(pubserver) = pubserver.as_ref() {
                    if let Err(e) = pubserver.archive_old_commands(days) {
                        error!("Failed to archive old Publication Server commands: {}", e)
                    }
                }
            }
        })
    });
    scheduler.watch_thread(Duration::from_millis(100))
}

#[cfg(feature = "multi-user")]
fn make_login_cache_sweeper_sh(cache: Arc<LoginSessionCache>) -> ScheduleHandle {
    let mut scheduler = clokwerk::Scheduler::new();
    scheduler.every(1.minutes()).run(move || {
        let mut rt = Runtime::new().unwrap();
        rt.block_on(async {
            debug!("Triggering background sweep of session decryption cache");
            
            if let Err(e) = cache.sweep() {
                error!("Background sweep of session decryption cache failed: {}", e);
            }
        })
    });
    scheduler.watch_thread(Duration::from_millis(100))
}