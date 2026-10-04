//! The session's model world: the config + model-factory pair, plus
//! the agent built from them — behind ONE shared cell so the
//! host-level world refresh (login/logout today, config reload when
//! it lands) is a state write at receive, the model register's
//! pattern: any thread swaps the cell's contents, every reader locks,
//! clones, and releases (no guard crosses an await), and the derived
//! agent cache lives INSIDE the cell so a refresh cannot leave a
//! stale build behind (clearing it is safe invalidation — the cache
//! is lazy, rebuilt at the next run open).

use super::{ModelFactory, Session};
use crate::lock::lock;
use std::sync::{Arc, Mutex};
use tabit_config::TabitConfig;
use tabit_engine::agent::Agent;
use tabit_protocol::ModelSelection;

/// The shared cell (see the module doc). Cheap to clone — the
/// session, its receive-time probes (the model probe, the register's
/// facts), and the endpoint's worker handle all hold one.
pub(crate) type SharedWorld = Arc<Mutex<SessionWorld>>;

/// The cell's contents.
pub(crate) struct SessionWorld {
    /// The current config — swapped by every refresh (login/logout
    /// keep the same config Arc; config reload, when it lands, swaps
    /// it for real).
    pub(crate) config: Arc<TabitConfig>,
    /// The current model factory (see `factory_custom`).
    pub(crate) factory: ModelFactory,
    /// Provenance: `true` when the factory came from
    /// [`SessionBuilder::model_factory`](super::SessionBuilder::model_factory)
    /// — the caller's own construction (an embedder's, a test's
    /// scripted mock), which a world refresh never replaces (the
    /// config still swaps). The registry-derived factory
    /// ([`SessionBuilder::world_factory`](super::SessionBuilder::world_factory),
    /// or the builder's default) tracks the world.
    factory_custom: bool,
    /// The built agent — a derived cache of (world, selection), not a
    /// second truth. Living in the cell makes the refresh's
    /// invalidation structural; `ensure_agent` (run open's
    /// point-of-use freshness check) is the one builder.
    pub(crate) agent: Option<(Arc<Agent>, ModelSelection)>,
}

impl SessionWorld {
    pub(crate) fn new(
        config: Arc<TabitConfig>,
        factory: ModelFactory,
        factory_custom: bool,
    ) -> SharedWorld {
        Arc::new(Mutex::new(Self {
            config,
            factory,
            factory_custom,
            agent: None,
        }))
    }

    /// The world refresh's session half: swap the config, swap the
    /// factory unless it is the session's own (custom provenance
    /// survives — an embedder's scripted factory is not the world's),
    /// and drop the cached agent (a derived cache; the next run open
    /// rebuilds it against the new world). In-flight runs are
    /// untouched by construction — they bound their agent at run
    /// open.
    pub(crate) fn refresh(&mut self, config: Arc<TabitConfig>, factory: ModelFactory) {
        self.config = config;
        if !self.factory_custom {
            self.factory = factory;
        }
        self.agent = None;
    }
}

impl Session {
    /// The session's world cell — the endpoint worker's refresh
    /// handle (the login/logout walk): the session itself is
    /// task-owned, but the world is a shared cell precisely so this
    /// state write can land at receive.
    pub(crate) fn world_cell(&self) -> SharedWorld {
        self.world.clone()
    }

    /// The current (config, factory) pair under ONE lock — run open's
    /// snapshot shape: two separate reads could straddle a refresh
    /// and hand a torn pair.
    pub(crate) fn world_snapshot(&self) -> (Arc<TabitConfig>, ModelFactory) {
        let world = lock(&self.world);
        (world.config.clone(), world.factory.clone())
    }

    /// The current config (lock, clone, release — the one read shape
    /// every consumer keeps).
    pub(crate) fn world_config(&self) -> Arc<TabitConfig> {
        lock(&self.world).config.clone()
    }

    /// The host-level world refresh, direct-`Session` form — the
    /// endpoint's build/refresh bracket (a session built but not yet
    /// in `workers`, which the refresh's worker walk missed) and the
    /// session tests ride this; resident workers ride
    /// [`Self::world_cell`] instead (the worker task owns the
    /// session). See [`SessionWorld::refresh`].
    pub(crate) fn refresh_world(&self, config: Arc<TabitConfig>, factory: ModelFactory) {
        lock(&self.world).refresh(config, factory);
    }
}
