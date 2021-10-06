//! Application state.

use std::{
    borrow::Cow, collections::HashSet, convert::TryInto, future::Future, mem,
    panic::AssertUnwindSafe, path::Path, time::Duration,
};

use anyhow::anyhow;
use derive_more::{Deref, Display, From, Into};
use ephyr_log::log;
use futures::{
    future::TryFutureExt as _,
    sink,
    stream::{StreamExt as _, TryStreamExt as _},
};
use futures_signals::signal::{Mutable, SignalExt as _};
use juniper::{
    graphql_scalar, GraphQLEnum, GraphQLObject, GraphQLScalarValue,
    GraphQLUnion, ParseScalarResult, ParseScalarValue, ScalarValue, Value,
};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{de::Error as _, Deserialize, Deserializer, Serialize};
use smart_default::SmartDefault;
use tokio::{fs, io::AsyncReadExt as _};
use url::Url;
use uuid::Uuid;

use crate::{display_panic, serde::is_false, spec, srs, Spec};

/// Server's settings.
///
/// It keeps different settings not related to restreams but to whole server
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct Settings {
    /// [`argon2`] hash of password which protects access to this application's
    /// public APIs.
    pub password_hash: Option<String>,

    /// [`argon2`] hash of password which protects access to single output
    /// application's public APIs.
    pub password_output_hash: Option<String>,

    /// Title for the server
    /// It is used for differentiating servers on UI side if multiple servers
    /// are used.
    pub title: Option<String>,

    /// Whether do we need to confirm deletion of inputs and outputs
    /// If `true` we should confirm deletion, `false` - do not confirm
    pub delete_confirmation: Option<bool>,

    /// Whether do we need to confirm enabling/disabling of inputs or outputs
    /// If `true` we should confirm, `false` - do not confirm
    pub enable_confirmation: Option<bool>,
}

impl Settings {
    /// Exports this [`Settings`] as a [`spec::v1::Settings`].
    #[inline]
    #[must_use]
    pub fn export(&self) -> spec::v1::Settings {
        spec::v1::Settings {
            delete_confirmation: self.delete_confirmation,
            enable_confirmation: self.enable_confirmation,
            title: self.title.clone(),
        }
    }

    // Applies the given [`spec::v1::Settings`] to this [`Settings`].
    ///
    pub fn apply(&mut self, new: spec::v1::Settings) {
        self.title = new.title;
        self.delete_confirmation = new.delete_confirmation;
        self.enable_confirmation = new.enable_confirmation;
    }
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            password_hash: None,
            password_output_hash: None,
            title: None,
            delete_confirmation: Some(true),
            enable_confirmation: Some(true),
        }
    }
}

/// Reactive application's state.
///
/// Any changes to it automatically propagate to the appropriate subscribers.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct State {
    /// Global [`Settings`] of server
    pub settings: Mutable<Settings>,

    /// All [`Restream`]s performed by this application.
    pub restreams: Mutable<Vec<Restream>>,
}

impl State {
    /// Instantiates a new [`State`] reading it from a `file` (if any) and
    /// performing all the required inner subscriptions.
    ///
    /// # Errors
    ///
    /// If [`State`] file exists, but fails to be parsed.
    pub async fn try_new<P: AsRef<Path>>(
        file: P,
    ) -> Result<Self, anyhow::Error> {
        let file = file.as_ref();

        let mut contents = vec![];
        let _ = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .read(true)
            .open(&file)
            .await
            .map_err(|e| {
                anyhow!("Failed to open '{}' file: {}", file.display(), e)
            })?
            .read_to_end(&mut contents)
            .await
            .map_err(|e| {
                anyhow!("Failed to read '{}' file: {}", file.display(), e)
            })?;

        let state = if contents.is_empty() {
            State::default()
        } else {
            serde_json::from_slice(&contents).map_err(|e| {
                anyhow!(
                    "Failed to deserialize state from '{}' file: {}",
                    file.display(),
                    e,
                )
            })?
        };

        let (file, persisted_state) = (file.to_owned(), state.clone());
        let persist_state1 = move || {
            fs::write(
                file.clone(),
                serde_json::to_vec(&persisted_state)
                    .expect("Failed to serialize server state"),
            )
            .map_err(|e| log::error!("Failed to persist server state: {}", e))
        };
        let persist_state2 = persist_state1.clone();
        Self::on_change("persist_restreams", &state.restreams, move |_| {
            persist_state1()
        });
        Self::on_change("persist_settings", &state.settings, move |_| {
            persist_state2()
        });

        Ok(state)
    }

    /// Applies the given [`Spec`] to this [`State`].
    ///
    /// If `replace` is `true` then all the [`Restream`]s, [`Restream::outputs`]
    /// and [`Output::mixins`] will be replaced with new ones, otherwise new
    /// ones will be merged with already existing ones.
    pub fn apply(&self, new: spec::v1::Spec, replace: bool) {
        let mut restreams = self.restreams.lock_mut();
        if replace {
            let mut olds = mem::replace(
                &mut *restreams,
                Vec::with_capacity(new.restreams.len()),
            );
            for new in new.restreams {
                if let Some(mut old) = olds
                    .iter()
                    .enumerate()
                    .find_map(|(n, o)| (o.key == new.key).then(|| n))
                    .map(|n| olds.swap_remove(n))
                {
                    old.apply(new, replace);
                    restreams.push(old);
                } else {
                    restreams.push(Restream::new(new));
                }
            }
        } else {
            for new in new.restreams {
                if let Some(old) =
                    restreams.iter_mut().find(|o| o.key == new.key)
                {
                    old.apply(new, replace);
                } else {
                    restreams.push(Restream::new(new));
                }
            }
        }

        let mut settings = self.settings.lock_mut();
        if new.settings.is_some() || replace {
            settings.apply(
                new.settings.unwrap_or_else(|| Settings::default().export()),
            );
        }
    }

    /// Exports this [`State`] as a [`spec::v1::Spec`].
    #[inline]
    #[must_use]
    pub fn export(&self) -> Spec {
        spec::v1::Spec {
            settings: Some(self.settings.get_cloned().export()),
            restreams: self
                .restreams
                .get_cloned()
                .iter()
                .map(Restream::export)
                .collect(),
        }
        .into()
    }

    /// Subscribes the specified `hook` to changes of the [`Mutable`] `val`ue.
    ///
    /// `name` is just a convenience for describing the `hook` in logs.
    pub fn on_change<F, Fut, T>(name: &'static str, val: &Mutable<T>, hook: F)
    where
        F: FnMut(T) -> Fut + Send + 'static,
        Fut: Future + Send + 'static,
        T: Clone + PartialEq + Send + Sync + 'static,
    {
        drop(tokio::spawn(
            AssertUnwindSafe(
                val.signal_cloned().dedupe_cloned().to_stream().then(hook),
            )
            .catch_unwind()
            .map_err(move |p| {
                log::crit!(
                    "Panicked executing `{}` hook of state: {}",
                    name,
                    display_panic(&p),
                );
            })
            .map(|_| Ok(()))
            .forward(sink::drain()),
        ));
    }

    /// Adds a new [`Restream`] by the given `spec` to this [`State`].
    ///
    /// # Errors
    ///
    /// If this [`State`] has a [`Restream`] with such `key` already.
    pub fn add_restream(&self, spec: spec::v1::Restream) -> anyhow::Result<()> {
        let mut restreams = self.restreams.lock_mut();

        if restreams.iter().any(|r| r.key == spec.key) {
            return Err(anyhow!("Restream.key '{}' is used already", spec.key));
        }

        restreams.push(Restream::new(spec));
        Ok(())
    }

    /// Edits a [`Restream`] with the given `spec` identified by the given `id`
    /// in this [`State`].
    ///
    /// Returns [`None`] if there is no [`Restream`] with such `id` in this
    /// [`State`].
    ///
    /// # Errors
    ///
    /// If this [`State`] has a [`Restream`] with such `key` already.
    pub fn edit_restream(
        &self,
        id: RestreamId,
        spec: spec::v1::Restream,
    ) -> anyhow::Result<Option<()>> {
        let mut restreams = self.restreams.lock_mut();

        if restreams.iter().any(|r| r.key == spec.key && r.id != id) {
            return Err(anyhow!("Restream.key '{}' is used already", spec.key));
        }

        #[allow(clippy::manual_find_map)] // due to consuming `spec`
        Ok(restreams
            .iter_mut()
            .find(|r| r.id == id)
            .map(|r| r.apply(spec, false)))
    }

    /// Removes a [`Restream`] with the given `id` from this [`State`].
    ///
    /// Returns [`None`] if there is no [`Restream`] with such `id` in this
    /// [`State`].
    #[allow(clippy::must_use_candidate)]
    pub fn remove_restream(&self, id: RestreamId) -> Option<()> {
        let mut restreams = self.restreams.lock_mut();
        let prev_len = restreams.len();
        restreams.retain(|r| r.id != id);
        (restreams.len() != prev_len).then(|| ())
    }

    /// Enables a [`Restream`] with the given `id` in this [`State`].
    ///
    /// Returns `true` if it has been enabled, or `false` if it already has been
    /// enabled, or [`None`] if it doesn't exist.
    #[must_use]
    pub fn enable_restream(&self, id: RestreamId) -> Option<bool> {
        self.restreams
            .lock_mut()
            .iter_mut()
            .find_map(|r| (r.id == id).then(|| r.input.enable()))
    }

    /// Disables a [`Restream`] with the given `id` in this [`State`].
    ///
    /// Returns `true` if it has been disabled, or `false` if it already has
    /// been disabled, or [`None`] if it doesn't exist.
    #[must_use]
    pub fn disable_restream(&self, id: RestreamId) -> Option<bool> {
        self.restreams
            .lock_mut()
            .iter_mut()
            .find_map(|r| (r.id == id).then(|| r.input.disable()))
    }

    /// Enables an [`Input`] with the given `id` in the specified [`Restream`]
    /// of this [`State`].
    ///
    /// Returns `true` if it has been enabled, or `false` if it already has been
    /// enabled, or [`None`] if it doesn't exist.
    #[must_use]
    pub fn enable_input(
        &self,
        id: InputId,
        restream_id: RestreamId,
    ) -> Option<bool> {
        self.restreams
            .lock_mut()
            .iter_mut()
            .find(|r| r.id == restream_id)?
            .input
            .find_mut(id)
            .map(Input::enable)
    }

    /// Disables an [`Input`] with the given `id` in the specified [`Restream`]
    /// of this [`State`].
    ///
    /// Returns `true` if it has been disabled, or `false` if it already has
    /// been disabled, or [`None`] if it doesn't exist.
    #[must_use]
    pub fn disable_input(
        &self,
        id: InputId,
        restream_id: RestreamId,
    ) -> Option<bool> {
        self.restreams
            .lock_mut()
            .iter_mut()
            .find(|r| r.id == restream_id)?
            .input
            .find_mut(id)
            .map(Input::disable)
    }

    ///
    ///
    /// Returns `true` if it has been disabled, or `false` if it already has
    /// been disabled, or [`None`] if it doesn't exist.
    #[must_use]
    pub fn change_endpoint_label(
        &self,
        id: InputId,
        restream_id: RestreamId,
        endpoint_id: EndpointId,
        label: Option<Label>,
    ) -> Option<bool> {
        self.restreams
            .lock_mut()
            .iter_mut()
            .find(|r| r.id == restream_id)?
            .input
            .find_mut(id)?
            .endpoints
            .iter_mut()
            .find(|endpoint| endpoint.id == endpoint_id)?
            .label = label;
        Some(true)
    }

    /// Adds a new [`Output`] to the specified [`Restream`] of this [`State`].
    ///
    /// Returns [`None`] if there is no [`Restream`] with such `id` in this
    /// [`State`].
    ///
    /// # Errors
    ///
    /// If the [`Restream`] has an [`Output`] with such `dst` already.
    pub fn add_output(
        &self,
        restream_id: RestreamId,
        spec: spec::v1::Output,
    ) -> anyhow::Result<Option<()>> {
        let mut restreams = self.restreams.lock_mut();

        let outputs = if let Some(r) =
            restreams.iter_mut().find(|r| r.id == restream_id)
        {
            &mut r.outputs
        } else {
            return Ok(None);
        };

        if let Some(o) = outputs.iter().find(|o| o.dst == spec.dst) {
            return Err(anyhow!("Output.dst '{}' is used already", o.dst));
        }

        outputs.push(Output::new(spec));
        Ok(Some(()))
    }

    /// Edits an [`Output`] with the given `spec` identified by the given `id`
    /// in the specified [`Restream`] of this [`State`].
    ///
    /// Returns [`None`] if there is no [`Restream`] with such `restream_id` in
    /// this [`State`], or there is no [`Output`] with such `id`.
    ///
    /// # Errors
    ///
    /// If the [`Restream`] has an [`Output`] with such `dst` already.
    pub fn edit_output(
        &self,
        restream_id: RestreamId,
        id: OutputId,
        spec: spec::v1::Output,
    ) -> anyhow::Result<Option<()>> {
        let mut restreams = self.restreams.lock_mut();

        let outputs = if let Some(r) =
            restreams.iter_mut().find(|r| r.id == restream_id)
        {
            &mut r.outputs
        } else {
            return Ok(None);
        };

        if outputs.iter().any(|o| o.dst == spec.dst && o.id != id) {
            return Err(anyhow!("Output.dst '{}' is used already", spec.dst));
        }

        #[allow(clippy::manual_find_map)] // due to consuming `spec`
        Ok(outputs
            .iter_mut()
            .find(|o| o.id == id)
            .map(|o| o.apply(spec, true)))
    }

    /// Removes an [`Output`] with the given `id` from the specified
    /// [`Restream`] of this [`State`].
    ///
    /// Returns [`None`] if there is no [`Restream`] with such `restream_id` or
    /// no [`Output`] with such `id` in this [`State`].
    #[must_use]
    pub fn remove_output(
        &self,
        id: OutputId,
        restream_id: RestreamId,
    ) -> Option<()> {
        let mut restreams = self.restreams.lock_mut();
        let outputs =
            &mut restreams.iter_mut().find(|r| r.id == restream_id)?.outputs;

        let prev_len = outputs.len();
        outputs.retain(|o| o.id != id);
        (outputs.len() != prev_len).then(|| ())
    }

    /// Enables an [`Output`] with the given `id` in the specified [`Restream`]
    /// of this [`State`].
    ///
    /// Returns `true` if it has been enabled, or `false` if it already has been
    /// enabled, or [`None`] if it doesn't exist.
    #[must_use]
    pub fn enable_output(
        &self,
        id: OutputId,
        restream_id: RestreamId,
    ) -> Option<bool> {
        let mut restreams = self.restreams.lock_mut();
        let output = restreams
            .iter_mut()
            .find(|r| r.id == restream_id)?
            .outputs
            .iter_mut()
            .find(|o| o.id == id)?;

        if output.enabled {
            return Some(false);
        }

        output.enabled = true;
        Some(true)
    }

    /// Disables an [`Output`] with the given `id` in the specified [`Restream`]
    /// of this [`State`].
    ///
    /// Returns `true` if it has been disabled, or `false` if it already has
    /// been disabled, or [`None`] if it doesn't exist.
    #[must_use]
    pub fn disable_output(
        &self,
        id: OutputId,
        restream_id: RestreamId,
    ) -> Option<bool> {
        let mut restreams = self.restreams.lock_mut();
        let output = restreams
            .iter_mut()
            .find(|r| r.id == restream_id)?
            .outputs
            .iter_mut()
            .find(|o| o.id == id)?;

        if !output.enabled {
            return Some(false);
        }

        output.enabled = false;
        Some(true)
    }

    /// Get [Output] from [Restream] by `restream_id` and `output_id`
    #[must_use]
    pub fn get_output(
        &self,
        restream_id: RestreamId,
        output_id: OutputId,
    ) -> Option<Output> {
        self.restreams
            .get_cloned()
            .into_iter()
            .find(|r| r.id == restream_id)?
            .outputs
            .into_iter()
            .find(|o| o.id == output_id)
    }

    /// Enables all [`Output`]s in the specified [`Restream`] of this [`State`].
    ///
    /// Returns `true` if at least one [`Output`] has been enabled, or `false`
    /// if all of them already have been enabled, or [`None`] if no [`Restream`]
    /// with such `restream_id` exists.
    #[must_use]
    pub fn enable_all_outputs(&self, restream_id: RestreamId) -> Option<bool> {
        self.set_state_of_all_outputs(restream_id, true)
    }

    /// Disables all [`Output`]s in the specified [`Restream`] of this
    /// [`State`].
    ///
    /// Returns `true` if at least one [`Output`] has been disabled, or `false`
    /// if all of them already have been disabled, or [`None`] if no
    /// [`Restream`] with such `restream_id` exists.
    #[must_use]
    pub fn disable_all_outputs(&self, restream_id: RestreamId) -> Option<bool> {
        self.set_state_of_all_outputs(restream_id, false)
    }

    /// Enables all [`Output`]s in all [`Restream`]s of this [`State`].
    ///
    /// Returns `true` if at least one [`Output`] has been enabled, or `false`
    /// if all of them already have been enabled or there are no outputs
    #[must_use]
    pub fn enable_all_outputs_of_restreams(&self) -> bool {
        self.set_state_of_all_outputs_of_restreams(true)
    }

    /// Disables all [`Output`]s in ALL [`Restream`]s of this [`State`].
    ///
    /// Returns `true` if at least one [`Output`] has been disabled, or `false`
    /// if all of them already have been disabled or there are no outputs
    #[must_use]
    pub fn disable_all_outputs_of_restreams(&self) -> bool {
        self.set_state_of_all_outputs_of_restreams(false)
    }

    /// Tunes a [`Volume`] rate of the specified [`Output`] or its [`Mixin`] in
    /// this [`State`].
    ///
    /// Returns `true` if a [`Volume`] rate has been changed, or `false` if it
    /// has the same value already.
    ///
    /// Returns [`None`] if no such [`Restream`]/[`Output`]/[`Mixin`] exists.
    #[must_use]
    pub fn tune_volume(
        &self,
        restream_id: RestreamId,
        output_id: OutputId,
        mixin_id: Option<MixinId>,
        volume: Volume,
    ) -> Option<bool> {
        let mut restreams = self.restreams.lock_mut();
        let output = restreams
            .iter_mut()
            .find(|r| r.id == restream_id)?
            .outputs
            .iter_mut()
            .find(|o| o.id == output_id)?;

        let curr_volume = if let Some(id) = mixin_id {
            &mut output.mixins.iter_mut().find(|m| m.id == id)?.volume
        } else {
            &mut output.volume
        };

        if *curr_volume == volume {
            return Some(false);
        }

        *curr_volume = volume;
        Some(true)
    }

    /// Tunes a [`Delay`] of the specified [`Mixin`] in this [`State`].
    ///
    /// Returns `true` if a [`Delay`] has been changed, or `false` if it has the
    /// same value already.
    ///
    /// Returns [`None`] if no such [`Restream`]/[`Output`]/[`Mixin`] exists.
    #[must_use]
    pub fn tune_delay(
        &self,
        input_id: RestreamId,
        output_id: OutputId,
        mixin_id: MixinId,
        delay: Delay,
    ) -> Option<bool> {
        let mut restreams = self.restreams.lock_mut();
        let mixin = restreams
            .iter_mut()
            .find(|r| r.id == input_id)?
            .outputs
            .iter_mut()
            .find(|o| o.id == output_id)?
            .mixins
            .iter_mut()
            .find(|m| m.id == mixin_id)?;

        if mixin.delay == delay {
            return Some(false);
        }

        mixin.delay = delay;
        Some(true)
    }

    /// Disables/Enables all [`Output`]s in the specified [`Restream`] of this
    /// [`State`].
    #[must_use]
    fn set_state_of_all_outputs(
        &self,
        restream_id: RestreamId,
        enabled: bool,
    ) -> Option<bool> {
        let mut restreams = self.restreams.lock_mut();
        Some(
            restreams
                .iter_mut()
                .find(|r| r.id == restream_id)?
                .outputs
                .iter_mut()
                .filter(|o| o.enabled != enabled)
                .fold(false, |_, o| {
                    o.enabled = enabled;
                    true
                }),
        )
    }

    /// Disables/Enables all [`Output`]s in ALL [`Restream`]s of this [`State`].
    #[must_use]
    fn set_state_of_all_outputs_of_restreams(&self, enabled: bool) -> bool {
        let mut restreams = self.restreams.lock_mut();
        restreams
            .iter_mut()
            .flat_map(|r| r.outputs.iter_mut())
            .filter(|o| o.enabled != enabled)
            .fold(false, |_, o| {
                o.enabled = enabled;
                true
            })
    }
}

/// Re-stream of a live stream from one `Input` to many `Output`s.
#[derive(
    Clone, Debug, Deserialize, Eq, GraphQLObject, PartialEq, Serialize,
)]
pub struct Restream {
    /// Unique ID of this `Input`.
    ///
    /// Once assigned, it never changes.
    pub id: RestreamId,

    /// Unique key of this `Restream` identifying it, and used to form its
    /// endpoints URLs.
    pub key: RestreamKey,

    /// Optional label of this `Restream`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<Label>,

    /// `Input` that a live stream is received from.
    pub input: Input,

    /// `Output`s that a live stream is re-streamed to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<Output>,
}

impl Restream {
    /// Creates a new [`Restream`] out of the given [`spec::v1::Restream`].
    #[inline]
    #[must_use]
    pub fn new(spec: spec::v1::Restream) -> Self {
        Self {
            id: RestreamId::random(),
            key: spec.key,
            label: spec.label,
            input: Input::new(spec.input),
            outputs: spec.outputs.into_iter().map(Output::new).collect(),
        }
    }

    /// Applies the given [`spec::v1::Restream`] to this [`Restream`].
    ///
    /// If `replace` is `true` then all the [`Restream::outputs`] will be
    /// replaced with new ones, otherwise new ones will be merged with already
    /// existing [`Restream::outputs`].
    pub fn apply(&mut self, new: spec::v1::Restream, replace: bool) {
        self.key = new.key;
        self.label = new.label;
        self.input.apply(new.input);
        if replace {
            let mut olds = mem::replace(
                &mut self.outputs,
                Vec::with_capacity(new.outputs.len()),
            );
            for new in new.outputs {
                if let Some(mut old) = olds
                    .iter()
                    .enumerate()
                    .find_map(|(n, o)| (o.dst == new.dst).then(|| n))
                    .map(|n| olds.swap_remove(n))
                {
                    old.apply(new, replace);
                    self.outputs.push(old);
                } else {
                    self.outputs.push(Output::new(new));
                }
            }
        } else {
            for new in new.outputs {
                if let Some(old) =
                    self.outputs.iter_mut().find(|o| o.dst == new.dst)
                {
                    old.apply(new, replace);
                } else {
                    self.outputs.push(Output::new(new));
                }
            }
        }
    }

    /// Exports this [`Restream`] as a [`spec::v1::Restream`].
    #[inline]
    #[must_use]
    pub fn export(&self) -> spec::v1::Restream {
        spec::v1::Restream {
            id: Some(self.id),
            key: self.key.clone(),
            label: self.label.clone(),
            input: self.input.export(),
            outputs: self.outputs.iter().map(Output::export).collect(),
        }
    }

    /// Returns an URL on a local [SRS] server of the endpoint representing a
    /// main [`Input`] in this [`Restream`].
    ///
    /// # Errors
    ///
    /// If not found any RTMP [`Input`] endpoint
    ///
    /// [SRS]: https://github.com/ossrs/srs
    pub fn main_input_rtmp_endpoint_url(&self) -> anyhow::Result<Url> {
        match self.input.endpoints.iter().find(|e| e.is_rtmp()) {
            Some(main) => Ok(main.kind.rtmp_url(&self.key, &self.input.key)),
            None => Err(anyhow!("Not found any RTMP endpoint")),
        }
    }
}

/// ID of a `Restream`.
#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Display,
    Eq,
    From,
    GraphQLScalarValue,
    Into,
    PartialEq,
    Serialize,
)]
pub struct RestreamId(Uuid);

impl RestreamId {
    /// Generates a new random [`RestreamId`].
    #[inline]
    #[must_use]
    pub fn random() -> Self {
        Self(Uuid::new_v4())
    }
}

/// Key of a [`Restream`] identifying it, and used to form its endpoints URLs.
#[derive(
    Clone, Debug, Deref, Display, Eq, Hash, Into, PartialEq, Serialize,
)]
pub struct RestreamKey(String);

impl RestreamKey {
    /// Creates a new [`RestreamKey`] if the given value meets its invariants.
    #[must_use]
    pub fn new<'s, S: Into<Cow<'s, str>>>(val: S) -> Option<Self> {
        static REGEX: Lazy<Regex> =
            Lazy::new(|| Regex::new("^[a-z0-9_-]{1,20}$").unwrap());

        let val = val.into();
        (!val.is_empty() && REGEX.is_match(&val))
            .then(|| Self(val.into_owned()))
    }
}

impl<'de> Deserialize<'de> for RestreamKey {
    #[inline]
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(<Cow<'_, str>>::deserialize(deserializer)?)
            .ok_or_else(|| D::Error::custom("Not a valid Restream.key"))
    }
}

/// Type of `Restream`'s `key` identifying it, and used to form its endpoints
/// URLs.
///
/// It should meet `[a-z0-9_-]{1,20}` format.
#[graphql_scalar]
impl<S> GraphQLScalar for RestreamKey
where
    S: ScalarValue,
{
    fn resolve(&self) -> Value {
        Value::scalar(self.0.as_str().to_owned())
    }

    fn from_input_value(v: &InputValue) -> Option<Self> {
        v.as_scalar()
            .and_then(ScalarValue::as_str)
            .and_then(Self::new)
    }

    fn from_str(value: ScalarToken<'_>) -> ParseScalarResult<'_, S> {
        <String as ParseScalarValue<S>>::from_str(value)
    }
}

impl PartialEq<str> for RestreamKey {
    #[inline]
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

/// Upstream source that a `Restream` receives a live stream from.
#[derive(
    Clone, Debug, Deserialize, Eq, GraphQLObject, PartialEq, Serialize,
)]
pub struct Input {
    /// Unique ID of this `Input`.
    ///
    /// Once assigned, it never changes.
    pub id: InputId,

    /// Key of this `Input` to expose its `InputEndpoint`s with for accepting
    /// and serving a live stream.
    pub key: InputKey,

    /// Endpoints of this `Input` serving a live stream for `Output`s and
    /// clients.
    pub endpoints: Vec<InputEndpoint>,

    /// Source to pull a live stream from.
    ///
    /// If specified, then this `Input` will pull a live stream from it (pull
    /// kind), otherwise this `Input` will await a live stream to be pushed
    /// (push kind).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub src: Option<InputSrc>,

    /// Indicator whether this `Input` is enabled, so is allowed to receive a
    /// live stream from its upstream sources.
    #[serde(default, skip_serializing_if = "is_false")]
    pub enabled: bool,
}

impl Input {
    /// Creates a new [`Input`] out of the given [`spec::v1::Input`].
    #[must_use]
    pub fn new(spec: spec::v1::Input) -> Self {
        Self {
            id: InputId::random(),
            key: spec.key,
            endpoints: spec
                .endpoints
                .into_iter()
                .map(InputEndpoint::new)
                .collect(),
            src: spec.src.map(InputSrc::new),
            enabled: spec.enabled,
        }
    }

    /// Applies the given [`spec::v1::Input`] to this [`Input`].
    pub fn apply(&mut self, new: spec::v1::Input) {
        if self.key != new.key
            || !new.enabled
            || (self.src.is_none() && new.src.is_some())
            || (self.src.is_some() && new.src.is_none())
        {
            // SRS endpoints have changed, disabled, or push/pull type has been
            // switched, so we should kick the publisher and all the players.
            for e in &mut self.endpoints {
                e.srs_publisher_id = None;
                e.srs_player_ids.clear();
            }
        }

        self.key = new.key;
        // Temporary omit changing existing `enabled` value to avoid unexpected
        // breakages of ongoing re-streams.
        //self.enabled = new.enabled;

        let mut olds = mem::replace(
            &mut self.endpoints,
            Vec::with_capacity(new.endpoints.len()),
        );
        for new in new.endpoints {
            if let Some(mut old) = olds
                .iter()
                .enumerate()
                .find_map(|(n, o)| (o.kind == new.kind).then(|| n))
                .map(|n| olds.swap_remove(n))
            {
                old.apply(new);
                self.endpoints.push(old);
            } else {
                self.endpoints.push(InputEndpoint::new(new));
            }
        }

        match (self.src.as_mut(), new.src) {
            (Some(old), Some(new)) => old.apply(new),
            (None, Some(new)) => self.src = Some(InputSrc::new(new)),
            _ => self.src = None,
        }
    }

    /// Exports this [`Input`] as a [`spec::v1::Input`].
    #[must_use]
    pub fn export(&self) -> spec::v1::Input {
        spec::v1::Input {
            id: Some(self.id),
            key: self.key.clone(),
            endpoints: self
                .endpoints
                .iter()
                .map(InputEndpoint::export)
                .collect(),
            src: self.src.as_ref().map(InputSrc::export),
            enabled: self.enabled,
        }
    }

    /// Enables this [`Input`].
    ///
    /// Returns `false` if it has been enabled already.
    #[must_use]
    pub fn enable(&mut self) -> bool {
        let mut changed = !self.enabled;

        self.enabled = true;

        if let Some(InputSrc::Failover(s)) = self.src.as_mut() {
            for i in &mut s.inputs {
                changed |= i.enable();
            }
        }

        changed
    }

    /// Disables this [`Input`].
    ///
    /// Returns `false` if it has been disabled already.
    #[must_use]
    pub fn disable(&mut self) -> bool {
        let mut changed = self.enabled;

        self.enabled = false;

        for e in &mut self.endpoints {
            e.srs_publisher_id = None;
            e.srs_player_ids.clear();
            // Do not rely only on SRS to set status, as it sporadically races.
            e.status = Status::Offline;
        }

        if let Some(InputSrc::Failover(s)) = self.src.as_mut() {
            for i in &mut s.inputs {
                changed |= i.disable();
            }
        }

        changed
    }

    /// Lookups for an [`Input`] with the given `id` inside this [`Input`] or
    /// its [`FailoverInputSrc::inputs`].
    #[must_use]
    pub fn find_mut(&mut self, id: InputId) -> Option<&mut Self> {
        if self.id == id {
            return Some(self);
        }
        if let Some(InputSrc::Failover(s)) = &mut self.src {
            s.inputs.iter_mut().find_map(|i| i.find_mut(id))
        } else {
            None
        }
    }

    /// Indicates whether this [`Input`] is ready to serve a live stream for
    /// [`Output`]s.
    #[must_use]
    pub fn is_ready_to_serve(&self) -> bool {
        let mut is_online = self
            .endpoints
            .iter()
            .any(|e| e.is_rtmp() && e.status == Status::Online);

        if !is_online {
            if let Some(InputSrc::Failover(s)) = &self.src {
                is_online = s.inputs.iter().any(|i| {
                    i.endpoints
                        .iter()
                        .any(|e| e.is_rtmp() && e.status == Status::Online)
                });
            }
        }

        is_online
    }
}

/// Endpoint of an `Input` serving a live stream for `Output`s and clients.
#[derive(
    Clone, Debug, Deserialize, Eq, GraphQLObject, PartialEq, Serialize,
)]
pub struct InputEndpoint {
    /// Unique ID of this `InputEndpoint`.
    ///
    /// Once assigned, it never changes.
    pub id: EndpointId,

    /// Kind of this `InputEndpoint`.
    pub kind: InputEndpointKind,

    /// User defined label for each Endpoint
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<Label>,

    /// `Status` of this `InputEndpoint` indicating whether it actually serves a
    /// live stream ready to be consumed by `Output`s and clients.
    #[serde(skip)]
    pub status: Status,

    /// ID of [SRS] client who publishes a live stream to this [`InputEndpoint`]
    /// (either an external client or a local process).
    ///
    /// [SRS]: https://github.com/ossrs/srs
    #[graphql(skip)]
    #[serde(skip)]
    pub srs_publisher_id: Option<srs::ClientId>,

    /// IDs of [SRS] clients who play a live stream from this [`InputEndpoint`]
    /// (either an external clients or a local processes).
    ///
    /// [SRS]: https://github.com/ossrs/srs
    #[graphql(skip)]
    #[serde(skip)]
    pub srs_player_ids: HashSet<srs::ClientId>,
}

impl InputEndpoint {
    /// Creates a new [`InputEndpoint`] out of the given
    /// [`spec::v1::InputEndpoint`].
    #[inline]
    #[must_use]
    pub fn new(spec: spec::v1::InputEndpoint) -> Self {
        Self {
            id: EndpointId::random(),
            kind: spec.kind,
            status: Status::Offline,
            label: None,
            srs_publisher_id: None,
            srs_player_ids: HashSet::new(),
        }
    }

    /// Applies the given [`spec::v1::InputEndpoint`] to this [`InputEndpoint`].
    #[inline]
    pub fn apply(&mut self, new: spec::v1::InputEndpoint) {
        self.kind = new.kind;
    }

    /// Exports this [`InputEndpoint`] as a [`spec::v1::InputEndpoint`].
    #[inline]
    #[must_use]
    pub fn export(&self) -> spec::v1::InputEndpoint {
        spec::v1::InputEndpoint { kind: self.kind }
    }

    /// Indicates whether this [`InputEndpoint`] is an
    /// [`InputEndpointKind::Rtmp`].
    #[inline]
    #[must_use]
    pub fn is_rtmp(&self) -> bool {
        matches!(self.kind, InputEndpointKind::Rtmp)
    }
}

/// Possible kinds of an `InputEndpoint`.
#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Display,
    Eq,
    From,
    GraphQLEnum,
    Hash,
    PartialEq,
    Serialize,
)]
#[serde(rename_all = "lowercase")]
pub enum InputEndpointKind {
    /// [RTMP] endpoint.
    ///
    /// Can accept a live stream and serve it for playing.
    ///
    /// [RTMP]: https://en.wikipedia.org/wiki/Real-Time_Messaging_Protocol
    #[display(fmt = "RTMP")]
    Rtmp,

    /// [HLS] endpoint.
    ///
    /// Only serves a live stream for playing and is not able to accept one.
    ///
    /// [HLS]: https://en.wikipedia.org/wiki/HTTP_Live_Streaming
    #[display(fmt = "HLS")]
    Hls,
}

impl InputEndpointKind {
    /// Returns RTMP URL on a local [SRS] server of this [`InputEndpointKind`]
    /// for the given `restream` and `input`.
    ///
    /// # Panics
    /// No panics, because [`RestreamKey`] and [`InputKey`] are validated.
    ///
    /// [SRS]: https://github.com/ossrs/srs
    #[must_use]
    pub fn rtmp_url(self, restream: &RestreamKey, input: &InputKey) -> Url {
        Url::parse(&format!(
            "rtmp://127.0.0.1:1935/{}{}/{}",
            restream,
            match self {
                Self::Rtmp => "",
                Self::Hls => "?vhost=hls",
            },
            input,
        ))
        .unwrap()
    }
}

/// ID of an `InputEndpoint`.
#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Display,
    Eq,
    From,
    GraphQLScalarValue,
    Into,
    PartialEq,
    Serialize,
)]
pub struct EndpointId(Uuid);

impl EndpointId {
    /// Generates a new random [`EndpointId`].
    #[inline]
    #[must_use]
    pub fn random() -> Self {
        Self(Uuid::new_v4())
    }
}

/// Source to pull a live stream by an `Input` from.
#[derive(
    Clone, Debug, Deserialize, Eq, From, GraphQLUnion, PartialEq, Serialize,
)]
#[serde(rename_all = "lowercase")]
pub enum InputSrc {
    /// Remote endpoint.
    Remote(RemoteInputSrc),

    /// Multiple local endpoints forming a failover source.
    Failover(FailoverInputSrc),
}

impl InputSrc {
    /// Creates a new [`InputSrc`] out of the given [`spec::v1::InputSrc`].
    #[inline]
    #[must_use]
    pub fn new(spec: spec::v1::InputSrc) -> Self {
        match spec {
            spec::v1::InputSrc::RemoteUrl(url) => {
                Self::Remote(RemoteInputSrc { url, label: None })
            }
            spec::v1::InputSrc::FailoverInputs(inputs) => {
                Self::Failover(FailoverInputSrc {
                    inputs: inputs.into_iter().map(Input::new).collect(),
                })
            }
        }
    }

    /// Applies the given [`spec::v1::InputSrc`] to this [`InputSrc`].
    ///
    /// Replaces all the [`FailoverInputSrc::inputs`] with new ones.
    pub fn apply(&mut self, new: spec::v1::InputSrc) {
        match (self, new) {
            (Self::Remote(old), spec::v1::InputSrc::RemoteUrl(new_url)) => {
                old.url = new_url;
            }
            (Self::Failover(src), spec::v1::InputSrc::FailoverInputs(news)) => {
                let mut olds = mem::replace(
                    &mut src.inputs,
                    Vec::with_capacity(news.len()),
                );
                for new in news {
                    if let Some(mut old) = olds
                        .iter()
                        .enumerate()
                        .find_map(|(n, o)| (o.key == new.key).then(|| n))
                        .map(|n| olds.swap_remove(n))
                    {
                        old.apply(new);
                        src.inputs.push(old);
                    } else {
                        src.inputs.push(Input::new(new));
                    }
                }
            }
            (old, new) => *old = Self::new(new),
        }
    }

    /// Exports this [`InputSrc`] as a [`spec::v1::InputSrc`].
    #[inline]
    #[must_use]
    pub fn export(&self) -> spec::v1::InputSrc {
        match self {
            Self::Remote(i) => spec::v1::InputSrc::RemoteUrl(i.url.clone()),
            Self::Failover(src) => spec::v1::InputSrc::FailoverInputs(
                src.inputs.iter().map(Input::export).collect(),
            ),
        }
    }
}

/// Remote upstream source to pull a live stream by an `Input` from.
#[derive(
    Clone, Debug, Deserialize, Eq, GraphQLObject, PartialEq, Serialize,
)]
pub struct RemoteInputSrc {
    /// URL of this `RemoteInputSrc`.
    pub url: InputSrcUrl,

    /// Label for this Endpoint
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<Label>,
}

/// Failover source of multiple `Input`s to pull a live stream by an `Input`
/// from.
#[derive(
    Clone, Debug, Deserialize, Eq, GraphQLObject, PartialEq, Serialize,
)]
pub struct FailoverInputSrc {
    /// `Input`s forming this `FailoverInputSrc`.
    ///
    /// Failover is implemented by attempting to pull the first `Input` falling
    /// back to the second one, and so on. Once the first source is restored,
    /// we pool from it once again.
    pub inputs: Vec<Input>,
}

/// ID of an `Input`.
#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Display,
    Eq,
    From,
    GraphQLScalarValue,
    Into,
    PartialEq,
    Serialize,
)]
pub struct InputId(Uuid);

impl InputId {
    /// Generates a new random [`InputId`].
    #[inline]
    #[must_use]
    pub fn random() -> Self {
        Self(Uuid::new_v4())
    }
}

/// Key of an [`Input`] used to form its endpoint URL.
#[derive(
    Clone, Debug, Deref, Display, Eq, Hash, Into, PartialEq, Serialize,
)]
pub struct InputKey(String);

impl InputKey {
    /// Creates a new [`InputKey`] if the given value meets its invariants.
    #[must_use]
    pub fn new<'s, S: Into<Cow<'s, str>>>(val: S) -> Option<Self> {
        static REGEX: Lazy<Regex> =
            Lazy::new(|| Regex::new("^[a-z0-9_-]{1,50}$").unwrap());

        let val = val.into();
        (!val.is_empty() && REGEX.is_match(&val))
            .then(|| Self(val.into_owned()))
    }
}

impl<'de> Deserialize<'de> for InputKey {
    #[inline]
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(<Cow<'_, str>>::deserialize(deserializer)?)
            .ok_or_else(|| D::Error::custom("Not a valid Input.key"))
    }
}

/// Type of `Input`'s `key` used to form its endpoint URL.
///
/// It should meet `[a-z0-9_-]{1,50}` format.
#[graphql_scalar]
impl<S> GraphQLScalar for InputKey
where
    S: ScalarValue,
{
    fn resolve(&self) -> Value {
        Value::scalar(self.0.as_str().to_owned())
    }

    fn from_input_value(v: &InputValue) -> Option<Self> {
        v.as_scalar()
            .and_then(ScalarValue::as_str)
            .and_then(Self::new)
    }

    fn from_str(value: ScalarToken<'_>) -> ParseScalarResult<'_, S> {
        <String as ParseScalarValue<S>>::from_str(value)
    }
}

impl PartialEq<str> for InputKey {
    #[inline]
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

/// [`Url`] of a [`RemoteInputSrc`].
///
/// Only the following URLs are allowed at the moment:
/// - [RTMP] URL (starting with `rtmp://` or `rtmps://` scheme and having a
///   host);
/// - [HLS] URL (starting with `http://` or `https://` scheme, having a host,
///   and with `.m3u8` extension in its path).
///
/// [HLS]: https://en.wikipedia.org/wiki/HTTP_Live_Streaming
/// [RTMP]: https://en.wikipedia.org/wiki/Real-Time_Messaging_Protocol
#[derive(
    Clone, Debug, Deref, Display, Eq, Hash, Into, PartialEq, Serialize,
)]
pub struct InputSrcUrl(Url);

impl InputSrcUrl {
    /// Creates a new [`InputSrcUrl`] if the given [`Url`] is suitable for that.
    ///
    /// # Errors
    ///
    /// Returns the given [`Url`] back if it doesn't represent a valid
    /// [`InputSrcUrl`].
    #[inline]
    pub fn new(url: Url) -> Result<Self, Url> {
        if Self::validate(&url) {
            Ok(Self(url))
        } else {
            Err(url)
        }
    }

    /// Validates the given [`Url`] to represent a valid [`InputSrcUrl`].
    #[must_use]
    pub fn validate(url: &Url) -> bool {
        match url.scheme() {
            "rtmp" | "rtmps" => url.has_host(),
            "http" | "https" => {
                url.has_host()
                    && Path::new(url.path()).extension()
                        == Some("m3u8".as_ref())
            }
            _ => false,
        }
    }
}

impl<'de> Deserialize<'de> for InputSrcUrl {
    #[inline]
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(Url::deserialize(deserializer)?).map_err(|url| {
            D::Error::custom(format!("Not a valid RemoteInputSrc.url: {}", url))
        })
    }
}

/// Type of a `RemoteInputSrc.url`.
///
/// Only the following URLs are allowed at the moment:
/// - [RTMP] URL (starting with `rtmp://` or `rtmps://` scheme and having a
///   host);
/// - [HLS] URL (starting with `http://` or `https://` scheme, having a host,
///   and with `.m3u8` extension in its path).
///
/// [HLS]: https://en.wikipedia.org/wiki/HTTP_Live_Streaming
/// [RTMP]: https://en.wikipedia.org/wiki/Real-Time_Messaging_Protocol
#[graphql_scalar]
impl<S> GraphQLScalar for InputSrcUrl
where
    S: ScalarValue,
{
    fn resolve(&self) -> Value {
        Value::scalar(self.0.as_str().to_owned())
    }

    fn from_input_value(v: &InputValue) -> Option<Self> {
        v.as_scalar()
            .and_then(ScalarValue::as_str)
            .and_then(|s| Url::parse(s).ok())
            .and_then(|url| Self::new(url).ok())
    }

    fn from_str(value: ScalarToken<'_>) -> ParseScalarResult<'_, S> {
        <String as ParseScalarValue<S>>::from_str(value)
    }
}

/// Downstream destination that a `Restream` re-streams a live stream to.
#[derive(
    Clone, Debug, Deserialize, Eq, GraphQLObject, PartialEq, Serialize,
)]
pub struct Output {
    /// Unique ID of this `Output`.
    ///
    /// Once assigned, it never changes.
    pub id: OutputId,

    /// Downstream URL to re-stream a live stream onto.
    ///
    /// At the moment only [RTMP] and [Icecast] are supported.
    ///
    /// [Icecast]: https://icecast.org
    /// [RTMP]: https://en.wikipedia.org/wiki/Real-Time_Messaging_Protocol
    pub dst: OutputDstUrl,

    /// Optional label of this `Output`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<Label>,

    /// Url of stream preview.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_url: Option<Url>,

    /// Volume rate of this `Output`'s audio tracks when mixed with
    /// `Output.mixins`.
    ///
    /// Has no effect when there is no `Output.mixins`.
    #[serde(default, skip_serializing_if = "Volume::is_origin")]
    pub volume: Volume,

    /// `Mixin`s to mix this `Output` with before re-streaming it to its
    /// downstream destination.
    ///
    /// If empty, then no mixing is performed and re-streaming is as cheap as
    /// possible (just copies bytes "as is").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mixins: Vec<Mixin>,

    /// Indicator whether this `Output` is enabled, so is allowed to perform a
    /// live stream re-streaming to its downstream destination.
    #[serde(default, skip_serializing_if = "is_false")]
    pub enabled: bool,

    /// `Status` of this `Output` indicating whether it actually re-streams a
    /// live stream to its downstream destination.
    #[serde(skip)]
    pub status: Status,
}

impl Output {
    /// Creates a new [`Output`] out of the given [`spec::v1::Output`].
    #[inline]
    #[must_use]
    pub fn new(spec: spec::v1::Output) -> Self {
        Self {
            id: OutputId::random(),
            dst: spec.dst,
            label: spec.label,
            preview_url: spec.preview_url,
            volume: spec.volume,
            mixins: spec.mixins.into_iter().map(Mixin::new).collect(),
            enabled: spec.enabled,
            status: Status::Offline,
        }
    }

    /// Applies the given [`spec::v1::Output`] to this [`Output`].
    ///
    /// If `replace` is `true` then all the [`Output::mixins`] will be replaced
    /// with new ones, otherwise new ones will be merged with already existing
    /// [`Output::mixins`].
    pub fn apply(&mut self, new: spec::v1::Output, replace: bool) {
        self.dst = new.dst;
        self.label = new.label;
        self.preview_url = new.preview_url;
        self.volume = new.volume;
        // Temporary omit changing existing `enabled` value to avoid unexpected
        // breakages of ongoing re-streams.
        //self.enabled = new.enabled;
        if replace {
            let mut olds = mem::replace(
                &mut self.mixins,
                Vec::with_capacity(new.mixins.len()),
            );
            for new in new.mixins {
                if let Some(mut old) = olds
                    .iter()
                    .enumerate()
                    .find_map(|(n, o)| (o.src == new.src).then(|| n))
                    .map(|n| olds.swap_remove(n))
                {
                    old.apply(new);
                    self.mixins.push(old);
                } else {
                    self.mixins.push(Mixin::new(new));
                }
            }
        } else {
            for new in new.mixins {
                if let Some(old) =
                    self.mixins.iter_mut().find(|o| o.src == new.src)
                {
                    old.apply(new);
                } else {
                    self.mixins.push(Mixin::new(new));
                }
            }
        }
    }

    /// Exports this [`Output`] as a [`spec::v1::Output`].
    #[inline]
    #[must_use]
    pub fn export(&self) -> spec::v1::Output {
        spec::v1::Output {
            id: Some(self.id),
            dst: self.dst.clone(),
            label: self.label.clone(),
            preview_url: self.preview_url.clone(),
            volume: self.volume,
            mixins: self.mixins.iter().map(Mixin::export).collect(),
            enabled: self.enabled,
        }
    }
}

/// ID of an `Output`.
#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Display,
    Eq,
    From,
    GraphQLScalarValue,
    Into,
    PartialEq,
    Serialize,
)]
pub struct OutputId(Uuid);

impl OutputId {
    /// Generates a new random [`OutputId`].
    #[inline]
    #[must_use]
    pub fn random() -> Self {
        Self(Uuid::new_v4())
    }
}

/// [`Url`] of an [`Output::dst`].
///
/// Only the following URLs are allowed at the moment:
/// - [RTMP] URL (starting with `rtmp://` or `rtmps://` scheme and having a
///   host);
/// - [SRT] URL (starting with `srt://` scheme and having a host);
/// - [Icecast] URL (starting with `icecast://` scheme and having a host);
/// - [FLV] file URL (starting with `file:///` scheme, without host and
///   subdirectories, and with `.flv` extension in its path).
///
/// [FLV]: https://en.wikipedia.org/wiki/Flash_Video
/// [Icecast]: https://icecast.org
/// [RTMP]: https://en.wikipedia.org/wiki/Real-Time_Messaging_Protocol
/// [SRT]: https://en.wikipedia.org/wiki/Secure_Reliable_Transport
#[derive(
    Clone, Debug, Deref, Display, Eq, Hash, Into, PartialEq, Serialize,
)]
pub struct OutputDstUrl(Url);

impl OutputDstUrl {
    /// Creates a new [`OutputDstUrl`] if the given [`Url`] is suitable for
    /// that.
    ///
    /// # Errors
    ///
    /// Returns the given [`Url`] back if it doesn't represent a valid
    /// [`OutputDstUrl`].
    #[inline]
    pub fn new(url: Url) -> Result<Self, Url> {
        if Self::validate(&url) {
            Ok(Self(url))
        } else {
            Err(url)
        }
    }

    /// Validates the given [`Url`] to represent a valid [`OutputDstUrl`].
    #[must_use]
    pub fn validate(url: &Url) -> bool {
        match url.scheme() {
            "icecast" | "rtmp" | "rtmps" | "srt" => url.has_host(),
            "file" => {
                let path = Path::new(url.path());
                !url.has_host()
                    && path.is_absolute()
                    && path.extension() == Some("flv".as_ref())
                    && path.parent() == Some("/".as_ref())
                    && !url.path().contains("/../")
            }
            _ => false,
        }
    }
}

impl<'de> Deserialize<'de> for OutputDstUrl {
    #[inline]
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(Url::deserialize(deserializer)?).map_err(|url| {
            D::Error::custom(format!("Not a valid Output.src URL: {}", url))
        })
    }
}

/// Type of an `Output.dst` URL.
///
/// Only the following URLs are allowed at the moment:
/// - [RTMP] URL (starting with `rtmp://` or `rtmps://` scheme and having a
///   host);
/// - [SRT] URL (starting with `srt://` scheme and having a host);
/// - [Icecast] URL (starting with `icecast://` scheme and having a host);
/// - [FLV] file URL (starting with `file:///` scheme, without host and
///   subdirectories, and with `.flv` extension in its path).
///
/// [FLV]: https://en.wikipedia.org/wiki/Flash_Video
/// [Icecast]: https://icecast.org
/// [RTMP]: https://en.wikipedia.org/wiki/Real-Time_Messaging_Protocol
/// [SRT]: https://en.wikipedia.org/wiki/Secure_Reliable_Transport
#[graphql_scalar]
impl<S> GraphQLScalar for OutputDstUrl
where
    S: ScalarValue,
{
    fn resolve(&self) -> Value {
        Value::scalar(self.0.as_str().to_owned())
    }

    fn from_input_value(v: &InputValue) -> Option<Self> {
        v.as_scalar()
            .and_then(ScalarValue::as_str)
            .and_then(|s| Url::parse(s).ok())
            .and_then(|url| Self::new(url).ok())
    }

    fn from_str(value: ScalarToken<'_>) -> ParseScalarResult<'_, S> {
        <String as ParseScalarValue<S>>::from_str(value)
    }
}

/// Additional source for an `Output` to be mixed with before re-streaming to
/// the destination.
#[derive(
    Clone, Debug, Deserialize, Eq, GraphQLObject, PartialEq, Serialize,
)]
pub struct Mixin {
    /// Unique ID of this `Mixin`.
    ///
    /// Once assigned, it never changes.
    pub id: MixinId,

    /// URL of the source to be mixed with an `Output`.
    ///
    /// At the moment, only [TeamSpeak] is supported.
    ///
    /// [TeamSpeak]: https://teamspeak.com
    pub src: MixinSrcUrl,

    /// Volume rate of this `Mixin`'s audio tracks to mix them with.
    #[serde(default, skip_serializing_if = "Volume::is_origin")]
    pub volume: Volume,

    /// Delay that this `Mixin` should wait before being mixed with an `Output`.
    ///
    /// Very useful to fix de-synchronization issues and correct timings between
    /// a `Mixin` and its `Output`.
    #[serde(default, skip_serializing_if = "Delay::is_zero")]
    pub delay: Delay,

    /// `Status` of this `Mixin` indicating whether it provides an actual media
    /// stream to be mixed with its `Output`.
    #[serde(skip)]
    pub status: Status,
}

impl Mixin {
    /// Creates a new [`Mixin`] out of the given [`spec::v1::Mixin`].
    #[inline]
    #[must_use]
    pub fn new(spec: spec::v1::Mixin) -> Self {
        Self {
            id: MixinId::random(),
            src: spec.src,
            volume: spec.volume,
            delay: spec.delay,
            status: Status::Offline,
        }
    }

    /// Applies the given [`spec::v1::Mixin`] to this [`Mixin`].
    #[inline]
    pub fn apply(&mut self, new: spec::v1::Mixin) {
        self.src = new.src;
        self.volume = new.volume;
        self.delay = new.delay;
    }

    /// Exports this [`Mixin`] as a [`spec::v1::Mixin`].
    #[inline]
    #[must_use]
    pub fn export(&self) -> spec::v1::Mixin {
        spec::v1::Mixin {
            src: self.src.clone(),
            volume: self.volume,
            delay: self.delay,
        }
    }
}

/// ID of a `Mixin`.
#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Display,
    Eq,
    From,
    GraphQLScalarValue,
    Into,
    PartialEq,
    Serialize,
)]
pub struct MixinId(Uuid);

impl MixinId {
    /// Generates a new random [`MixinId`].
    #[inline]
    #[must_use]
    pub fn random() -> Self {
        Self(Uuid::new_v4())
    }
}

/// [`Url`] of a [`Mixin::src`].
///
/// Only the following URLs are allowed at the moment:
/// - [TeamSpeak] URL (starting with `ts://` scheme and having a host);
/// - [MP3] HTTP URL (starting with `http://` or `https://` scheme, having a
///   host and `.mp3` extension in its path).
///
/// [MP3]: https://en.wikipedia.org/wiki/MP3
/// [TeamSpeak]: https://teamspeak.com
#[derive(
    Clone, Debug, Deref, Display, Eq, Hash, Into, PartialEq, Serialize,
)]
pub struct MixinSrcUrl(Url);

impl MixinSrcUrl {
    /// Creates a new [`MixinSrcUrl`] if the given [`Url`] is suitable for that.
    ///
    /// # Errors
    ///
    /// Returns the given [`Url`] back if it doesn't represent a valid
    /// [`MixinSrcUrl`].
    #[inline]
    pub fn new(url: Url) -> Result<Self, Url> {
        if Self::validate(&url) {
            Ok(Self(url))
        } else {
            Err(url)
        }
    }

    /// Validates the given [`Url`] to represent a valid [`MixinSrcUrl`].
    #[must_use]
    pub fn validate(url: &Url) -> bool {
        url.has_host()
            && match url.scheme() {
                "ts" => true,
                "http" | "https" => {
                    Path::new(url.path()).extension() == Some("mp3".as_ref())
                }
                _ => false,
            }
    }
}

impl<'de> Deserialize<'de> for MixinSrcUrl {
    #[inline]
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(Url::deserialize(deserializer)?).map_err(|url| {
            D::Error::custom(format!("Not a valid Mixin.src URL: {}", url))
        })
    }
}

/// Type of a `Mixin.src` URL.
///
/// Only the following URLs are allowed at the moment:
/// - [TeamSpeak] URL (starting with `ts://` scheme and having a host);
/// - [MP3] HTTP URL (starting with `http://` or `https://` scheme, having a
///   host and `.mp3` extension in its path).
///
/// [MP3]: https://en.wikipedia.org/wiki/MP3
/// [TeamSpeak]: https://teamspeak.com
#[graphql_scalar]
impl<S> GraphQLScalar for MixinSrcUrl
where
    S: ScalarValue,
{
    fn resolve(&self) -> Value {
        Value::scalar(self.0.as_str().to_owned())
    }

    fn from_input_value(v: &InputValue) -> Option<Self> {
        v.as_scalar()
            .and_then(ScalarValue::as_str)
            .and_then(|s| Url::parse(s).ok())
            .and_then(|url| Self::new(url).ok())
    }

    fn from_str(value: ScalarToken<'_>) -> ParseScalarResult<'_, S> {
        <String as ParseScalarValue<S>>::from_str(value)
    }
}

/// Specifies kind of password
#[derive(Clone, Copy, Debug, Eq, GraphQLEnum, PartialEq)]
pub enum PasswordKind {
    /// Password for main application
    Main,

    /// Password for single output application
    Output,
}

/// Status indicating availability of an `Input`, `Output`, or a `Mixin`.
#[derive(Clone, Copy, Debug, Eq, GraphQLEnum, PartialEq, SmartDefault)]
pub enum Status {
    /// Inactive, no operations are performed and no media traffic is flowed.
    #[default]
    Offline,

    /// Initializing, media traffic doesn't yet flow as expected.
    Initializing,

    /// Active, all operations are performing successfully and media traffic
    /// flows as expected.
    Online,
}

/// Label of a [`Restream`] or an [`Output`].
#[derive(Clone, Debug, Deref, Display, Eq, Into, PartialEq, Serialize)]
pub struct Label(String);

impl Label {
    /// Creates a new [`Label`] if the given value meets its invariants.
    #[must_use]
    pub fn new<'s, S: Into<Cow<'s, str>>>(val: S) -> Option<Self> {
        static REGEX: Lazy<Regex> =
            Lazy::new(|| Regex::new(r"^[^,\n\t\r\f\v]{1,70}$").unwrap());

        let val = val.into();
        (!val.is_empty() && REGEX.is_match(&val))
            .then(|| Self(val.into_owned()))
    }
}

impl<'de> Deserialize<'de> for Label {
    #[inline]
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(<Cow<'_, str>>::deserialize(deserializer)?)
            .ok_or_else(|| D::Error::custom("Not a valid Label"))
    }
}

/// Type of a `Restream` or an `Output` label.
///
/// It should meet `[^,\n\t\r\f\v]{1,70}` format.
#[graphql_scalar]
impl<S> GraphQLScalar for Label
where
    S: ScalarValue,
{
    fn resolve(&self) -> Value {
        Value::scalar(self.0.as_str().to_owned())
    }

    fn from_input_value(v: &InputValue) -> Option<Self> {
        v.as_scalar()
            .and_then(ScalarValue::as_str)
            .and_then(Self::new)
    }

    fn from_str(value: ScalarToken<'_>) -> ParseScalarResult<'_, S> {
        <String as ParseScalarValue<S>>::from_str(value)
    }
}

/// Volume rate of an audio track in percents.
#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Eq,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize,
    SmartDefault,
)]
pub struct Volume(#[default(Self::ORIGIN.0)] u16);

impl Volume {
    /// Maximum possible value of a [`Volume`] rate.
    pub const MAX: Volume = Volume(1000);

    /// Value of a [`Volume`] rate corresponding to the original one of an audio
    /// track.
    pub const ORIGIN: Volume = Volume(100);

    /// Minimum possible value of a [`Volume`] rate. Actually, disables audio.
    pub const OFF: Volume = Volume(0);

    /// Creates a new [`Volume`] rate value if it satisfies the required
    /// invariants:
    /// - within [`Volume::OFF`] and [`Volume::MAX`] values.
    #[must_use]
    pub fn new<N: TryInto<u16>>(num: N) -> Option<Self> {
        let num = num.try_into().ok()?;
        if (Self::OFF.0..=Self::MAX.0).contains(&num) {
            Some(Self(num))
        } else {
            None
        }
    }

    /// Displays this [`Volume`] as a fraction of `1`, i.e. `100%` as `1`, `50%`
    /// as `0.50`, and so on.
    #[must_use]
    pub fn display_as_fraction(self) -> String {
        format!("{}.{:02}", self.0 / 100, self.0 % 100)
    }

    /// Indicates whether this [`Volume`] rate value corresponds is the
    /// [`Volume::ORIGIN`]al one.
    #[allow(clippy::trivially_copy_pass_by_ref)] // required for `serde`
    #[inline]
    #[must_use]
    pub fn is_origin(&self) -> bool {
        *self == Self::ORIGIN
    }
}

/// Type a volume rate of audio track in percents.
///
/// It's values are always within range of `0` and `1000` (inclusively).
///
/// `0` means disabled audio.
#[graphql_scalar]
impl<S> GraphQLScalar for Volume
where
    S: ScalarValue,
{
    fn resolve(&self) -> Value {
        Value::scalar(i32::from(self.0))
    }

    fn from_input_value(v: &InputValue) -> Option<Self> {
        v.as_scalar()
            .and_then(ScalarValue::as_int)
            .and_then(Self::new)
    }

    fn from_str(value: ScalarToken<'_>) -> ParseScalarResult<'_, S> {
        <String as ParseScalarValue<S>>::from_str(value)
    }
}

/// Delay of a [`Mixin`] being mixed with an [`Output`].
#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Default,
    Eq,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize,
)]
pub struct Delay(#[serde(with = "serde_humantime")] Duration);

impl Delay {
    /// Creates a new [`Delay`] out of the given milliseconds.
    #[inline]
    #[must_use]
    pub fn from_millis<N: TryInto<u64>>(millis: N) -> Option<Self> {
        millis
            .try_into()
            .ok()
            .map(|m| Self(Duration::from_millis(m)))
    }

    /// Returns milliseconds of this [`Delay`].
    #[inline]
    #[must_use]
    #[allow(clippy::missing_panics_doc)]
    pub fn as_millis(&self) -> i32 {
        self.0.as_millis().try_into().unwrap()
    }

    /// Indicates whether this [`Delay`] introduces no actual delay.
    #[inline]
    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.0 == Duration::default()
    }
}

/// Type of a `Mixin` delay in milliseconds.
///
/// Negative values are not allowed.
#[graphql_scalar]
impl<S> GraphQLScalar for Delay
where
    S: ScalarValue,
{
    fn resolve(&self) -> Value {
        Value::scalar(self.as_millis())
    }

    fn from_input_value(v: &InputValue) -> Option<Self> {
        v.as_scalar()
            .and_then(ScalarValue::as_int)
            .and_then(Self::from_millis)
    }

    fn from_str(value: ScalarToken<'_>) -> ParseScalarResult<'_, S> {
        <String as ParseScalarValue<S>>::from_str(value)
    }
}

#[cfg(test)]
mod volume_spec {
    use super::Volume;

    #[test]
    fn displays_as_fraction() {
        for (input, expected) in &[
            (1, "0.01"),
            (10, "0.10"),
            (200, "2.00"),
            (107, "1.07"),
            (170, "1.70"),
            (1000, "10.00"),
        ] {
            let actual = Volume::new(*input).unwrap().display_as_fraction();
            assert_eq!(&actual, *expected);
        }
    }
}
