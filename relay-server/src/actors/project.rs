use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use smallvec::SmallVec;
use tokio::time::Instant;
use url::Url;

use relay_auth::PublicKey;
use relay_common::{ProjectId, ProjectKey};
use relay_config::Config;
use relay_filter::{matches_any_origin, FiltersConfig};
use relay_general::pii::{DataScrubbingConfig, PiiConfig};
use relay_general::store::{BreakdownsConfig, MeasurementsConfig, TransactionNameRule};
use relay_general::types::SpanAttribute;
use relay_metrics::{Bucket, InsertMetrics, MergeBuckets, Metric, MetricsContainer};
use relay_quotas::{Quota, RateLimits, Scoping};
use relay_sampling::SamplingConfig;
use relay_statsd::metric;
use relay_system::BroadcastChannel;

use crate::actors::envelopes::{EnvelopeManager, SendMetrics};
use crate::actors::outcome::{DiscardReason, Outcome};
use crate::actors::processor::{EnvelopeProcessor, ProcessEnvelope};
use crate::actors::project_cache::{
    AddSamplingState, CheckedEnvelope, ProjectCache, RequestUpdate,
};
use crate::envelope::Envelope;
use crate::extractors::RequestMeta;
use crate::metrics_extraction::sessions::SessionMetricsConfig;
use crate::metrics_extraction::transactions::TransactionMetricsConfig;
use crate::metrics_extraction::TaggingRule;
use crate::service::Registry;
use crate::statsd::RelayCounters;
use crate::utils::{self, EnvelopeContext, EnvelopeLimiter, ErrorBoundary, MetricsLimiter};

#[cfg(feature = "processing")]
use crate::actors::processor::RateLimitFlushBuckets;

/// The expiry status of a project state. Return value of [`ProjectState::check_expiry`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
enum Expiry {
    /// The project state is perfectly up to date.
    Updated,
    /// The project state is outdated but events depending on this project state can still be
    /// processed. The state should be refreshed in the background though.
    Stale,
    /// The project state is completely outdated and events need to be buffered up until the new
    /// state has been fetched.
    Expired,
}

/// The expiry status of a project state, together with the state itself if it has not expired.
/// Return value of [`Project::expiry_state`].
pub enum ExpiryState {
    /// An up-to-date project state. See [`Expiry::Updated`].
    Updated(Arc<ProjectState>),
    /// A stale project state that can still be used. See [`Expiry::Stale`].
    Stale(Arc<ProjectState>),
    /// An expired project state that should not be used. See [`Expiry::Expired`].
    Expired,
}

/// Sender type for messages that respond with project states.
pub type ProjectSender = relay_system::BroadcastSender<Arc<ProjectState>>;

/// Features exposed by project config.
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Feature {
    /// Enables ingestion and normalization of profiles.
    #[serde(rename = "organizations:profiling")]
    Profiling,
    /// Enables ingestion of Session Replays (Replay Recordings and Replay Events).
    #[serde(rename = "organizations:session-replay")]
    Replays,
    /// Enable transaction names normalization.
    /// Replacing UUIDs, SHAs and numerical IDs by placeholders.
    #[serde(rename = "organizations:transaction-name-normalize")]
    TransactionNameNormalize,

    /// Unused.
    ///
    /// This used to control the initial experimental metrics extraction for sessions and has been
    /// discontinued.
    #[serde(rename = "organizations:metrics-extraction")]
    Deprecated1,

    /// Forward compatibility.
    #[serde(other)]
    Unknown,
}

/// These are config values that the user can modify in the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProjectConfig {
    /// URLs that are permitted for cross original JavaScript requests.
    pub allowed_domains: Vec<String>,
    /// List of relay public keys that are permitted to access this project.
    pub trusted_relays: Vec<PublicKey>,
    /// Configuration for PII stripping.
    pub pii_config: Option<PiiConfig>,
    /// The grouping configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grouping_config: Option<Value>,
    /// Configuration for filter rules.
    #[serde(skip_serializing_if = "FiltersConfig::is_empty")]
    pub filter_settings: FiltersConfig,
    /// Configuration for data scrubbers.
    #[serde(skip_serializing_if = "DataScrubbingConfig::is_disabled")]
    pub datascrubbing_settings: DataScrubbingConfig,
    /// Maximum event retention for the organization.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_retention: Option<u16>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub quotas: Vec<Quota>,
    /// Configuration for sampling traces, if not present there will be no sampling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dynamic_sampling: Option<SamplingConfig>,
    /// Configuration for measurements.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measurements: Option<MeasurementsConfig>,
    /// Configuration for operation breakdown. Will be emitted only if present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breakdowns_v2: Option<BreakdownsConfig>,
    /// Configuration for extracting metrics from sessions.
    #[serde(skip_serializing_if = "SessionMetricsConfig::is_disabled")]
    pub session_metrics: SessionMetricsConfig,
    /// Configuration for extracting metrics from transaction events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transaction_metrics: Option<ErrorBoundary<TransactionMetricsConfig>>,
    /// The span attributes configuration.
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub span_attributes: BTreeSet<SpanAttribute>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub metric_conditional_tagging: Vec<TaggingRule>,
    /// Exposable features enabled for this project.
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub features: BTreeSet<Feature>,
    /// Transaction renaming rules.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tx_name_rules: Vec<TransactionNameRule>,
}

impl Default for ProjectConfig {
    fn default() -> Self {
        ProjectConfig {
            allowed_domains: vec!["*".to_string()],
            trusted_relays: vec![],
            pii_config: None,
            grouping_config: None,
            filter_settings: FiltersConfig::default(),
            datascrubbing_settings: DataScrubbingConfig::default(),
            event_retention: None,
            quotas: Vec::new(),
            dynamic_sampling: None,
            measurements: None,
            breakdowns_v2: None,
            session_metrics: SessionMetricsConfig::default(),
            transaction_metrics: None,
            span_attributes: BTreeSet::new(),
            metric_conditional_tagging: Vec::new(),
            features: BTreeSet::new(),
            tx_name_rules: Vec::new(),
        }
    }
}

/// These are config values that are passed to external Relays.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase", remote = "ProjectConfig")]
pub struct LimitedProjectConfig {
    pub allowed_domains: Vec<String>,
    pub trusted_relays: Vec<PublicKey>,
    pub pii_config: Option<PiiConfig>,
    /// Configuration for filter rules.
    #[serde(skip_serializing_if = "FiltersConfig::is_empty")]
    pub filter_settings: FiltersConfig,
    #[serde(skip_serializing_if = "DataScrubbingConfig::is_disabled")]
    pub datascrubbing_settings: DataScrubbingConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dynamic_sampling: Option<SamplingConfig>,
    #[serde(skip_serializing_if = "SessionMetricsConfig::is_disabled")]
    pub session_metrics: SessionMetricsConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transaction_metrics: Option<ErrorBoundary<TransactionMetricsConfig>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub metric_conditional_tagging: Vec<TaggingRule>,
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub span_attributes: BTreeSet<SpanAttribute>,
    /// Configuration for measurements.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measurements: Option<MeasurementsConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breakdowns_v2: Option<BreakdownsConfig>,
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub features: BTreeSet<Feature>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tx_name_rules: Vec<TransactionNameRule>,
}

/// The project state is a cached server state of a project.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectState {
    /// Unique identifier of this project.
    pub project_id: Option<ProjectId>,
    /// The timestamp of when the state was last changed.
    ///
    /// This might be `None` in some rare cases like where states
    /// are faked locally.
    #[serde(default)]
    pub last_change: Option<DateTime<Utc>>,
    /// Indicates that the project is disabled.
    #[serde(default)]
    pub disabled: bool,
    /// A container of known public keys in the project.
    ///
    /// Since version 2, each project state corresponds to a single public key. For this reason,
    /// only a single key can occur in this list.
    #[serde(default)]
    pub public_keys: SmallVec<[PublicKeyConfig; 1]>,
    /// The project's slug if available.
    #[serde(default)]
    pub slug: Option<String>,
    /// The project's current config.
    #[serde(default)]
    pub config: ProjectConfig,
    /// The organization id.
    #[serde(default)]
    pub organization_id: Option<u64>,

    /// The time at which this project state was last updated.
    #[serde(skip, default = "Instant::now")]
    pub last_fetch: Instant,

    /// True if this project state failed fetching or was incompatible with this Relay.
    #[serde(skip, default)]
    pub invalid: bool,
}

/// Controls how we serialize a ProjectState for an external Relay
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase", remote = "ProjectState")]
pub struct LimitedProjectState {
    pub project_id: Option<ProjectId>,
    pub last_change: Option<DateTime<Utc>>,
    pub disabled: bool,
    pub public_keys: SmallVec<[PublicKeyConfig; 1]>,
    pub slug: Option<String>,
    #[serde(with = "LimitedProjectConfig")]
    pub config: ProjectConfig,
    pub organization_id: Option<u64>,
}

impl ProjectState {
    /// Project state for a missing project.
    pub fn missing() -> Self {
        ProjectState {
            project_id: None,
            last_change: None,
            disabled: true,
            public_keys: SmallVec::new(),
            slug: None,
            config: ProjectConfig::default(),
            organization_id: None,
            last_fetch: Instant::now(),
            invalid: false,
        }
    }

    /// Project state for an unknown but allowed project.
    ///
    /// This state is used for forwarding in Proxy mode.
    pub fn allowed() -> Self {
        let mut state = ProjectState::missing();
        state.disabled = false;
        state
    }

    /// Project state for a deserialization error.
    pub fn err() -> Self {
        let mut state = ProjectState::missing();
        state.invalid = true;
        state
    }

    /// Returns configuration options for the public key.
    pub fn get_public_key_config(&self) -> Option<&PublicKeyConfig> {
        self.public_keys.get(0)
    }

    /// Returns `true` if the entire project should be considered
    /// disabled (blackholed, deleted etc.).
    pub fn disabled(&self) -> bool {
        self.disabled
    }

    /// Returns `true` if the project state obtained from the upstream could not be parsed. This
    /// results in events being dropped similar to disabled states, but can provide separate
    /// metrics.
    pub fn invalid(&self) -> bool {
        self.invalid
    }

    /// Returns whether this state is outdated and needs to be refetched.
    fn check_expiry(&self, config: &Config) -> Expiry {
        let expiry = match self.project_id {
            None => config.cache_miss_expiry(),
            Some(_) => config.project_cache_expiry(),
        };

        let elapsed = self.last_fetch.elapsed();
        if elapsed >= expiry + config.project_grace_period() {
            Expiry::Expired
        } else if elapsed >= expiry {
            Expiry::Stale
        } else {
            Expiry::Updated
        }
    }

    /// Returns the project config.
    pub fn config(&self) -> &ProjectConfig {
        &self.config
    }

    /// Returns `true` if the given project ID matches this project.
    ///
    /// If the project state has not been loaded, this check is skipped because the project
    /// identifier is not yet known. Likewise, this check is skipped for the legacy store endpoint
    /// which comes without a project ID. The id is later overwritten in `check_envelope`.
    pub fn is_valid_project_id(&self, stated_id: Option<ProjectId>, config: &Config) -> bool {
        match (self.project_id, stated_id, config.override_project_ids()) {
            (Some(actual_id), Some(stated_id), false) => actual_id == stated_id,
            _ => true,
        }
    }

    /// Checks if this origin is allowed for this project.
    fn is_valid_origin(&self, origin: Option<&Url>) -> bool {
        // Generally accept any event without an origin.
        let origin = match origin {
            Some(origin) => origin,
            None => return true,
        };

        // Match against list of allowed origins. If the list is empty we always reject.
        let allowed = &self.config().allowed_domains;
        if allowed.is_empty() {
            return false;
        }

        let allowed: Vec<_> = allowed
            .iter()
            .map(|origin| origin.as_str().into())
            .collect();

        matches_any_origin(Some(origin.as_str()), &allowed)
    }

    /// Returns `true` if the given public key matches this state.
    ///
    /// This is a sanity check since project states are keyed by the DSN public key. Unless the
    /// state is invalid or unloaded, it must always match the public key.
    pub fn is_matching_key(&self, project_key: ProjectKey) -> bool {
        if let Some(key_config) = self.get_public_key_config() {
            // Always validate if we have a key config.
            key_config.public_key == project_key
        } else {
            // Loaded states must have a key config, but ignore missing and invalid states.
            self.project_id.is_none()
        }
    }

    /// Amends request `Scoping` with information from this project state.
    ///
    /// This scoping amends `RequestMeta::get_partial_scoping` by adding organization and key info.
    /// The processor must fetch the full scoping before attempting to rate limit with partial
    /// scoping.
    ///
    /// To get the own scoping of this ProjectKey without amending request information, use
    /// [`Project::scoping`] instead.
    pub fn scope_request(&self, meta: &RequestMeta) -> Scoping {
        let mut scoping = meta.get_partial_scoping();

        // The key configuration may be missing if the event has been queued for extended times and
        // project was refetched in between. In such a case, access to key quotas is not availabe,
        // but we can gracefully execute all other rate limiting.
        scoping.key_id = self
            .get_public_key_config()
            .and_then(|config| config.numeric_id);

        // The original project identifier is part of the DSN. If the DSN was moved to another
        // project, the actual project identifier is different and can be obtained from project
        // states. This is only possible when the project state has been loaded.
        if let Some(project_id) = self.project_id {
            scoping.project_id = project_id;
        }

        // This is a hack covering three cases:
        //  1. Relay has not fetched the project state. In this case we have no way of knowing
        //     which organization this project belongs to and we need to ignore any
        //     organization-wide rate limits stored globally. This project state cannot hold
        //     organization rate limits yet.
        //  2. The state has been loaded, but the organization_id is not available. This is only
        //     the case for legacy Sentry servers that do not reply with organization rate
        //     limits. Thus, the organization_id doesn't matter.
        //  3. An organization id is available and can be matched against rate limits. In this
        //     project, all organizations will match automatically, unless the organization id
        //     has changed since the last fetch.
        scoping.organization_id = self.organization_id.unwrap_or(0);

        scoping
    }

    /// Returns quotas declared in this project state.
    pub fn get_quotas(&self) -> &[Quota] {
        self.config.quotas.as_slice()
    }

    /// Returns `Err` if the project is known to be invalid or disabled.
    ///
    /// If this project state is hard outdated, this returns `Ok(())`, instead, to avoid prematurely
    /// dropping data.
    pub fn check_disabled(&self, config: &Config) -> Result<(), DiscardReason> {
        // if the state is out of date, we proceed as if it was still up to date. The
        // upstream relay (or sentry) will still filter events.
        if self.check_expiry(config) == Expiry::Expired {
            return Ok(());
        }

        // if we recorded an invalid project state response from the upstream (i.e. parsing
        // failed), discard the event with a state reason.
        if self.invalid() {
            return Err(DiscardReason::ProjectState);
        }

        // only drop events if we know for sure the project or key are disabled.
        if self.disabled() {
            return Err(DiscardReason::ProjectId);
        }

        Ok(())
    }

    /// Determines whether the given request should be accepted or discarded.
    ///
    /// Returns `Ok(())` if the request should be accepted. Returns `Err(DiscardReason)` if the
    /// request should be discarded, by indicating the reason. The checks preformed for this are:
    ///
    ///  - Allowed origin headers
    ///  - Disabled or unknown projects
    ///  - Disabled project keys (DSN)
    pub fn check_request(&self, meta: &RequestMeta, config: &Config) -> Result<(), DiscardReason> {
        // Verify that the stated project id in the DSN matches the public key used to retrieve this
        // project state.
        if !self.is_valid_project_id(meta.project_id(), config) {
            return Err(DiscardReason::ProjectId);
        }

        // Try to verify the request origin with the project config.
        if !self.is_valid_origin(meta.origin()) {
            return Err(DiscardReason::Cors);
        }

        // sanity-check that the state has a matching public key loaded.
        if !self.is_matching_key(meta.public_key()) {
            relay_log::error!("public key mismatch on state {}", meta.public_key());
            return Err(DiscardReason::ProjectId);
        }

        // Check for invalid or disabled projects.
        self.check_disabled(config)?;

        Ok(())
    }

    /// Validates data in this project state and removes values that are partially invalid.
    pub fn sanitize(mut self) -> Self {
        self.config.quotas.retain(Quota::is_valid);
        self
    }

    pub fn has_feature(&self, feature: Feature) -> bool {
        self.config.features.contains(&feature)
    }
}

/// Represents a public key received from the projectconfig endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicKeyConfig {
    /// Public part of key (random hash).
    pub public_key: ProjectKey,

    /// The primary key of the DSN in Sentry's main database.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub numeric_id: Option<u64>,
}

struct StateChannel {
    inner: BroadcastChannel<Arc<ProjectState>>,
    no_cache: bool,
}

impl StateChannel {
    pub fn new() -> Self {
        Self {
            inner: BroadcastChannel::new(),
            no_cache: false,
        }
    }

    pub fn no_cache(&mut self, no_cache: bool) -> &mut Self {
        self.no_cache = no_cache;
        self
    }
}

enum GetOrFetch<'a> {
    Cached(Arc<ProjectState>),
    Scheduled(&'a mut StateChannel),
}

/// Structure representing organization and project configuration for a project key.
///
/// This structure no longer uniquely identifies a project. Instead, it identifies a project key.
/// Projects can define multiple keys, in which case this structure is duplicated for each instance.
pub struct Project {
    last_updated_at: Instant,
    project_key: ProjectKey,
    config: Arc<Config>,
    state: Option<Arc<ProjectState>>,
    state_channel: Option<StateChannel>,
    pending_validations: VecDeque<(Box<Envelope>, EnvelopeContext)>,
    pending_sampling: VecDeque<ProcessEnvelope>,
    rate_limits: RateLimits,
    last_no_cache: Instant,
}

impl Project {
    /// Creates a new `Project`.
    pub fn new(key: ProjectKey, config: Arc<Config>) -> Self {
        Project {
            last_updated_at: Instant::now(),
            project_key: key,
            config,
            state: None,
            state_channel: None,
            pending_validations: VecDeque::new(),
            pending_sampling: VecDeque::new(),
            rate_limits: RateLimits::new(),
            last_no_cache: Instant::now(),
        }
    }

    /// If we know that a project is disabled, disallow metrics, too.
    fn metrics_allowed(&self) -> bool {
        if let Some(state) = self.valid_state() {
            state.check_disabled(&self.config).is_ok()
        } else {
            // Projects without state go back to the original state of allowing metrics.
            true
        }
    }

    pub fn merge_rate_limits(&mut self, rate_limits: RateLimits) {
        self.rate_limits.merge(rate_limits);
    }

    /// Returns the current [`ExpiryState`] for this project.
    /// If the project state's [`Expiry`] is `Expired`, do not return it.
    pub fn expiry_state(&self) -> ExpiryState {
        match self.state {
            Some(ref state) => match state.check_expiry(self.config.as_ref()) {
                Expiry::Updated => ExpiryState::Updated(state.clone()),
                Expiry::Stale => ExpiryState::Stale(state.clone()),
                Expiry::Expired => ExpiryState::Expired,
            },
            None => ExpiryState::Expired,
        }
    }

    /// Returns the project state if it is not expired.
    ///
    /// Convenience wrapper around [`expiry_state`](Self::expiry_state).
    fn valid_state(&self) -> Option<Arc<ProjectState>> {
        match self.expiry_state() {
            ExpiryState::Updated(state) => Some(state),
            ExpiryState::Stale(state) => Some(state),
            ExpiryState::Expired => None,
        }
    }

    /// The rate limits that are active for this project.
    pub fn rate_limits(&self) -> &RateLimits {
        &self.rate_limits
    }

    /// The last time the project state was updated
    pub fn last_updated_at(&self) -> Instant {
        self.last_updated_at
    }

    /// Refresh the update time of the project in order to delay eviction.
    ///
    /// Called by the project cache when the project state is refreshed.
    pub fn refresh_updated_timestamp(&mut self) {
        self.last_updated_at = Instant::now();
    }

    /// Applies cached rate limits to the given metrics or metrics buckets.
    ///
    /// This only applies the rate limits currently stored on the project.
    fn rate_limit_metrics<T: MetricsContainer>(&self, metrics: Vec<T>) -> Vec<T> {
        match (&self.state, self.scoping()) {
            (Some(state), Some(scoping)) => {
                match MetricsLimiter::create(metrics, &state.config.quotas, scoping) {
                    Ok(mut limiter) => {
                        limiter.enforce_limits(Ok(&self.rate_limits));
                        limiter.into_metrics()
                    }
                    Err(metrics) => metrics,
                }
            }
            _ => metrics,
        }
    }

    /// Inserts given [buckets](Bucket) into the metrics aggregator.
    ///
    /// The buckets will be keyed underneath this project key.
    pub fn merge_buckets(&mut self, buckets: Vec<Bucket>) {
        if self.metrics_allowed() {
            let buckets = self.rate_limit_metrics(buckets);
            if !buckets.is_empty() {
                Registry::aggregator().send(MergeBuckets::new(self.project_key, buckets));
            }
        }
    }

    /// Inserts given [metrics](Metric) into the metrics aggregator.
    ///
    /// The metrics will be keyed underneath this project key.
    pub fn insert_metrics(&mut self, metrics: Vec<Metric>) {
        if self.metrics_allowed() {
            let metrics = self.rate_limit_metrics(metrics);
            if !metrics.is_empty() {
                Registry::aggregator().send(InsertMetrics::new(self.project_key, metrics));
            }
        }
    }

    /// Triggers a debounced refresh of the project state.
    ///
    /// If the state is already being updated in the background, this method checks if the request
    /// needs to be upgraded with the `no_cache` flag to ensure a more recent update.
    fn fetch_state(&mut self, no_cache: bool) -> &mut StateChannel {
        // If there is a running request and we do not need to upgrade it to no_cache, skip
        // scheduling a new fetch.
        let should_fetch =
            !matches!(self.state_channel, Some(ref channel) if channel.no_cache || !no_cache);
        let channel = self.state_channel.get_or_insert_with(StateChannel::new);

        if should_fetch {
            channel.no_cache(no_cache);
            relay_log::debug!("project {} state requested", self.project_key);
            ProjectCache::from_registry().send(RequestUpdate::new(self.project_key, no_cache));
        }

        channel
    }

    fn get_or_fetch_state(&mut self, mut no_cache: bool) -> GetOrFetch<'_> {
        // count number of times we are looking for the project state
        metric!(counter(RelayCounters::ProjectStateGet) += 1);

        // Allow at most 1 no_cache request per second. Gracefully degrade to cached requests.
        if no_cache {
            if self.last_no_cache.elapsed() < Duration::from_secs(1) {
                no_cache = false;
            } else {
                metric!(counter(RelayCounters::ProjectStateNoCache) += 1);
                self.last_no_cache = Instant::now();
            }
        }

        let cached_state = match self.expiry_state() {
            // Never use the cached state if `no_cache` is set.
            _ if no_cache => None,
            // There is no project state that can be used, fetch a state and return it.
            ExpiryState::Expired => None,
            // The project is semi-outdated, fetch new state but return old one.
            ExpiryState::Stale(state) => Some(state),
            // The project is not outdated, return early here to jump over fetching logic below.
            ExpiryState::Updated(state) => return GetOrFetch::Cached(state),
        };

        let channel = self.fetch_state(no_cache);

        match cached_state {
            Some(state) => GetOrFetch::Cached(state),
            None => GetOrFetch::Scheduled(channel),
        }
    }

    /// Returns the cached project state if it is valid.
    ///
    /// Depending on the state of the cache, this method takes different action:
    ///
    ///  - If the cached state is up-to-date, this method simply returns `Some`.
    ///  - If the cached state is stale, this method triggers a refresh in the background and
    ///    returns `Some`. The stale period can be configured through
    ///    [`Config::project_grace_period`].
    ///  - If there is no cached state or the cached state is fully outdated, this method triggers a
    ///    refresh in the background and returns `None`.
    ///
    /// If `no_cache` is set to true, this method always returns `None` and always triggers a
    /// background refresh.
    ///
    /// To wait for a valid state instead, use [`get_state`](Self::get_state).
    pub fn get_cached_state(&mut self, no_cache: bool) -> Option<Arc<ProjectState>> {
        match self.get_or_fetch_state(no_cache) {
            GetOrFetch::Cached(state) => Some(state),
            GetOrFetch::Scheduled(_) => None,
        }
    }

    /// Obtains a valid project state and passes it to the sender once ready.
    ///
    /// This first checks if the state needs to be updated. This is the case if the project state
    /// has passed its cache timeout. The `no_cache` flag forces an update. This does nothing if an
    /// update is already running in the background.
    ///
    /// Independent of updating, _stale_ states are passed to the sender immediately as long as they
    /// are in the [grace period](Config::project_grace_period).
    pub fn get_state(&mut self, sender: ProjectSender, no_cache: bool) {
        match self.get_or_fetch_state(no_cache) {
            GetOrFetch::Cached(state) => sender.send(state),
            GetOrFetch::Scheduled(channel) => channel.inner.attach(sender),
        }
    }

    /// Ensures the project state gets updated.
    ///
    /// This first checks if the state needs to be updated. This is the case if the project state
    /// has passed its cache timeout. The `no_cache` flag forces another update unless one is
    /// already running in the background.
    ///
    /// If an update is required, the update will start in the background and complete at a later
    /// point. Therefore, this method is useful to trigger an update early if it is already clear
    /// that the project state will be needed soon. To retrieve an updated state, use
    /// [`Project::get_state`] instead.
    pub fn prefetch(&mut self, no_cache: bool) {
        self.get_cached_state(no_cache);
    }

    /// Validates the envelope and submits the envelope to the next stage.
    ///
    /// If this project is disabled or rate limited, corresponding items are dropped from the
    /// envelope. Remaining items in the Envelope are forwarded:
    ///  - If the envelope needs dynamic sampling, this sends [`AddSamplingState`] to the
    ///    [`ProjectCache`] to add the required project state.
    ///  - Otherwise, the envelope is directly submitted to the [`EnvelopeProcessor`].
    fn flush_validation(
        &mut self,
        envelope: Box<Envelope>,
        envelope_context: EnvelopeContext,
        project_state: Arc<ProjectState>,
    ) {
        if let Ok(checked) = self.check_envelope(envelope, envelope_context) {
            if let Some((envelope, envelope_context)) = checked.envelope {
                let mut process = ProcessEnvelope {
                    envelope,
                    envelope_context,
                    project_state,
                    sampling_project_state: None,
                };

                if let Some(sampling_key) = utils::get_sampling_key(&process.envelope) {
                    let own_key = process
                        .project_state
                        .get_public_key_config()
                        .map(|c| c.public_key);

                    if Some(sampling_key) == own_key {
                        process.sampling_project_state = Some(process.project_state.clone());
                        EnvelopeProcessor::from_registry().send(process);
                    } else {
                        ProjectCache::from_registry()
                            .send(AddSamplingState::new(sampling_key, process));
                    }
                } else {
                    EnvelopeProcessor::from_registry().send(process);
                }
            }
        }
    }

    /// Enqueues an envelope for validation.
    ///
    /// If the project state is up to date, the message will be immediately to the next stage.
    /// Otherwise, this queues the envelope and flushes it when the project has been updated.
    ///
    /// This method will trigger an update of the project state internally if the state is stale or
    /// outdated.
    pub fn enqueue_validation(&mut self, envelope: Box<Envelope>, context: EnvelopeContext) {
        match self.get_cached_state(envelope.meta().no_cache()) {
            Some(state) => self.flush_validation(envelope, context, state),
            None => self.pending_validations.push_back((envelope, context)),
        }
    }

    /// Adds the project state for dynamic sampling and submits the Envelope for processing.
    fn flush_sampling(&self, mut message: ProcessEnvelope) {
        // Intentionally ignore all errors. Fallback sampling behavior applies in this case.
        if let Some(state) = self.valid_state() {
            // Never use rules from another organization.
            if state.organization_id == message.project_state.organization_id {
                message.sampling_project_state = Some(state);
            }
        }

        EnvelopeProcessor::from_registry().send(message);
    }

    /// Enqueues an envelope for adding a dynamic sampling project state.
    ///
    /// If the project state is up to date, the message will be immediately submitted for
    /// processing. Otherwise, this queues the envelope and flushes it when the project has been
    /// updated.
    ///
    /// This method will trigger an update of the project state internally if the state is stale or
    /// outdated.
    pub fn enqueue_sampling(&mut self, message: ProcessEnvelope) {
        match self.get_cached_state(message.envelope.meta().no_cache()) {
            Some(_) => self.flush_sampling(message),
            None => self.pending_sampling.push_back(message),
        }
    }

    /// Replaces the internal project state with a new one and triggers pending actions.
    ///
    /// This flushes pending envelopes from [`ValidateEnvelope`] and [`AddSamplingState`] and
    /// notifies all pending receivers from [`get_state`](Self::get_state).
    ///
    /// `no_cache` should be passed from the requesting call. Updates with `no_cache` will always
    /// take precedence.
    ///
    /// [`ValidateEnvelope`]: crate::actors::project_cache::ValidateEnvelope
    pub fn update_state(&mut self, mut state: Arc<ProjectState>, no_cache: bool) {
        let channel = match self.state_channel.take() {
            Some(channel) => channel,
            None => return,
        };

        // If the channel has `no_cache` set but we are not a `no_cache` request, we have
        // been superseeded. Put it back and let the other request take precedence.
        if channel.no_cache && !no_cache {
            self.state_channel = Some(channel);
            return;
        }

        match self.expiry_state() {
            // If the new state is invalid but the old one still usable, keep the old one.
            ExpiryState::Updated(old) | ExpiryState::Stale(old) if state.invalid() => state = old,
            // If the new state is valid or the old one is expired, always use the new one.
            _ => self.state = Some(state.clone()),
        }

        // Flush all queued `ValidateEnvelope` messages
        while let Some((envelope, context)) = self.pending_validations.pop_front() {
            self.flush_validation(envelope, context, state.clone());
        }

        // Flush all queued `AddSamplingState` messages
        while let Some(message) = self.pending_sampling.pop_front() {
            self.flush_sampling(message);
        }

        // Flush all waiting recipients.
        relay_log::debug!("project state {} updated", self.project_key);
        channel.inner.send(state);
    }

    /// Creates `Scoping` for this project if the state is loaded.
    ///
    /// Returns `Some` if the project state has been fetched and contains a project identifier,
    /// otherwise `None`.
    ///
    /// NOTE: This function does not check the expiry of the project state.
    pub fn scoping(&self) -> Option<Scoping> {
        let state = self.state.as_deref()?;
        Some(Scoping {
            organization_id: state.organization_id.unwrap_or(0),
            project_id: state.project_id?,
            project_key: self.project_key,
            key_id: state
                .get_public_key_config()
                .and_then(|config| config.numeric_id),
        })
    }

    pub fn check_envelope(
        &mut self,
        mut envelope: Box<Envelope>,
        mut envelope_context: EnvelopeContext,
    ) -> Result<CheckedEnvelope, DiscardReason> {
        let state = self.valid_state();
        let mut scoping = envelope_context.scoping();

        if let Some(ref state) = state {
            scoping = state.scope_request(envelope.meta());
            envelope_context.scope(scoping);

            if let Err(reason) = state.check_request(envelope.meta(), &self.config) {
                envelope_context.reject(Outcome::Invalid(reason));
                return Err(reason);
            }
        }

        self.rate_limits.clean_expired();

        let config = state.as_deref().map(|s| &s.config);
        let quotas = state.as_deref().map(|s| s.get_quotas()).unwrap_or(&[]);
        let envelope_limiter = EnvelopeLimiter::new(config, |item_scoping, _| {
            Ok(self.rate_limits.check_with_quotas(quotas, item_scoping))
        });

        let (enforcement, rate_limits) = envelope_limiter.enforce(&mut envelope, &scoping)?;
        enforcement.track_outcomes(&envelope, &scoping);
        envelope_context.update(&envelope);

        let envelope = if envelope.is_empty() {
            // Individual rate limits have already been issued above
            envelope_context.reject(Outcome::RateLimited(None));
            None
        } else {
            Some((envelope, envelope_context))
        };

        Ok(CheckedEnvelope {
            envelope,
            rate_limits,
        })
    }

    pub fn flush_buckets(&mut self, partition_key: Option<u64>, buckets: Vec<Bucket>) {
        let config = self.config.clone();

        // Schedule an update to the project state if it is outdated, regardless of whether the
        // metrics can be forwarded or not. We never wait for this update.
        let Some(project_state) = self.get_cached_state(false) else {
            relay_log::trace!("project expired: merging back {} buckets", buckets.len());
            // If the state is outdated, we need to wait for an updated state. Put them back into
            // the aggregator.
            Registry::aggregator().send(MergeBuckets::new(self.project_key, buckets));
            return;
        };

        let Some(scoping) = self.scoping() else {
            relay_log::trace!("there is no scoping: merging back {} buckets", buckets.len());
            Registry::aggregator().send(MergeBuckets::new(self.project_key, buckets));
            return;
        };

        // Only send if the project state is valid, otherwise drop this bucket.
        if project_state.check_disabled(config.as_ref()).is_err() {
            return;
        }

        // Check rate limits if necessary:
        let quotas = project_state.config.quotas.clone();
        let buckets = match MetricsLimiter::create(buckets, quotas, scoping) {
            Ok(mut bucket_limiter) => {
                let cached_rate_limits = self.rate_limits().clone();
                #[allow(unused_variables)]
                let was_rate_limited = bucket_limiter.enforce_limits(Ok(&cached_rate_limits));

                #[cfg(feature = "processing")]
                if !was_rate_limited && config.processing_enabled() {
                    // If there were no cached rate limits active, let the processor check redis:
                    EnvelopeProcessor::from_registry().send(RateLimitFlushBuckets {
                        bucket_limiter,
                        partition_key,
                    });

                    return;
                }

                bucket_limiter.into_metrics()
            }
            Err(buckets) => buckets,
        };

        if !buckets.is_empty() {
            EnvelopeManager::from_registry().send(SendMetrics {
                buckets,
                scoping,
                partition_key,
            });
        }
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let count = self.pending_validations.len() + self.pending_sampling.len();
        if count > 0 {
            relay_log::with_scope(
                |scope| scope.set_tag("project_key", self.project_key),
                || relay_log::error!("dropped project with {} envelopes", count),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use relay_common::{ProjectId, ProjectKey, UnixTimestamp};
    use relay_metrics::{Bucket, BucketValue, Metric, MetricValue};
    use serde_json::json;

    use super::{Config, Project, ProjectState, StateChannel};

    #[test]
    fn get_state_expired() {
        for expiry in [9999, 0] {
            let config = Arc::new(
                Config::from_json_value(json!(
                    {
                        "cache": {
                            "project_expiry": expiry,
                            "project_grace_period": 0,
                            "eviction_interval": 9999 // do not evict
                        }
                    }
                ))
                .unwrap(),
            );

            // Initialize project with a state
            let project_key = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fee").unwrap();
            let mut project_state = ProjectState::allowed();
            project_state.project_id = Some(ProjectId::new(123));
            let mut project = Project::new(project_key, config.clone());
            project.state = Some(Arc::new(project_state));

            // Direct access should always yield a state:
            assert!(project.state.is_some());

            if expiry > 0 {
                // With long expiry, should get a state
                assert!(project.valid_state().is_some());
            } else {
                // With 0 expiry, project should expire immediately. No state can be set.
                assert!(project.valid_state().is_none());
            }
        }
    }

    #[test]
    fn test_stale_cache() {
        let config = Arc::new(
            Config::from_json_value(json!(
                {
                    "cache": {
                        "project_expiry": 100,
                        "project_grace_period": 0,
                        "eviction_interval": 9999 // do not evict
                    }
                }
            ))
            .unwrap(),
        );

        let channel = StateChannel::new();

        // Initialize project with a state.
        let project_key = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fee").unwrap();
        let mut project_state = ProjectState::allowed();
        project_state.project_id = Some(ProjectId::new(123));
        let mut project = Project::new(project_key, config);
        project.state_channel = Some(channel);
        project.state = Some(Arc::new(project_state));

        // The project ID must be set.
        assert!(!project.state.as_ref().unwrap().invalid());
        // Try to update project with errored project state.
        project.update_state(Arc::new(ProjectState::err()), false);
        // Since we got invalid project state we still keep the old one meaning there
        // still must be the project id set.
        assert!(!project.state.as_ref().unwrap().invalid());
    }

    fn create_project(config: Option<serde_json::Value>) -> Project {
        let project_key = ProjectKey::parse("a94ae32be2584e0bbd7a4cbb95971fee").unwrap();
        let mut project = Project::new(project_key, Arc::new(Config::default()));
        let mut project_state = ProjectState::allowed();
        project_state.project_id = Some(ProjectId::new(42));
        if let Some(config) = config {
            project_state.config = serde_json::from_value(config).unwrap();
        }
        project.state = Some(Arc::new(project_state));
        project
    }

    fn create_transaction_metric() -> Metric {
        Metric {
            name: "d:transactions/foo".to_string(),
            value: MetricValue::Counter(1.0),
            timestamp: UnixTimestamp::now(),
            tags: Default::default(),
        }
    }

    #[test]
    fn test_rate_limit_incoming_metrics() {
        let project = create_project(None);
        let metrics = project.rate_limit_metrics(vec![create_transaction_metric()]);

        assert!(metrics.len() == 1);
    }

    #[test]
    fn test_rate_limit_incoming_metrics_no_quota() {
        let project = create_project(Some(json!({
            "quotas": [{
               "id": "foo",
               "categories": ["transaction"],
               "window": 3600,
               "limit": 0,
               "reasonCode": "foo",
           }]
        })));

        let metrics = project.rate_limit_metrics(vec![create_transaction_metric()]);

        assert!(metrics.is_empty());
    }

    fn create_transaction_bucket() -> Bucket {
        Bucket {
            name: "d:transactions/foo".to_string(),
            value: BucketValue::Counter(1.0),
            timestamp: UnixTimestamp::now(),
            tags: Default::default(),
            width: 10,
        }
    }

    #[test]
    fn test_rate_limit_incoming_buckets() {
        let project = create_project(None);
        let metrics = project.rate_limit_metrics(vec![create_transaction_bucket()]);

        assert!(metrics.len() == 1);
    }

    #[test]
    fn test_rate_limit_incoming_buckets_no_quota() {
        let project = create_project(Some(json!({
            "quotas": [{
               "id": "foo",
               "categories": ["transaction"],
               "window": 3600,
               "limit": 0,
               "reasonCode": "foo",
           }]
        })));

        let metrics = project.rate_limit_metrics(vec![create_transaction_bucket()]);

        assert!(metrics.is_empty());
    }
}
