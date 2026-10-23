//! Starting up, and switching to a new model.
//!
//! A start loads the model the vector cache says is active (so a new build keeps answering with
//! the model whose vectors it has) or, on a fresh cache, downloads and loads the manifest's. When
//! the manifest names another model, [`Service::update`] downloads it, re-embeds every note under
//! its id while the old one keeps answering, switches the answer in one step, and only then
//! deletes the old rows and files. A failure at any point leaves the old model serving, and the
//! next start or the next hourly try picks up from the vectors already written.

use std::sync::Arc;
use std::thread;

use super::backfill::{Halt, Report};
use super::engine::Resident;
use super::manifest::Manifest;
use super::{Active, Event, Service, locked, model};

/// A point of a model update, for [`Service::set_checkpoint`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The new model's files are in place.
    Downloaded,
    /// The new model is loaded.
    Loaded,
    /// A note is about to be embedded under the new model.
    Reembed,
    /// Every note has a vector under the new model.
    Reembedded,
    /// The new model answers searches.
    Switched,
    /// The old model's vectors are deleted.
    RowsDeleted,
    /// The old model's files are deleted.
    FilesDeleted,
}

impl Service {
    /// Gets a model loaded and answering: the active one if the cache has one whose files are
    /// there, else the manifest's (downloaded when missing). Emits `ready`. A model older than the
    /// manifest is switched by [`Service::update`] afterwards.
    pub fn prepare(&self) -> Result<(), String> {
        let target = self.opts.manifest;
        let active = locked(&self.cache).active().map(str::to_owned);

        let serving = match active {
            Some(id) if id == target.id => {
                let dir = self.download(&target)?;
                self.load(&id, dir, target.spec())?
            }
            Some(id) => match model::installed(&model::model_dir(&self.opts.root, &id)) {
                Some(spec) => {
                    *locked(&self.pending) = Some(target.id.to_owned());
                    self.load(&id, model::model_dir(&self.opts.root, &id), spec)?
                }
                None => {
                    // Its files are gone, so its vectors cannot be told apart from a query.
                    locked(&self.cache).delete_model(&id)?;
                    let dir = self.download(&target)?;
                    self.load(target.id, dir, target.spec())?
                }
            },
            None => {
                let dir = self.download(&target)?;
                self.load(target.id, dir, target.spec())?
            }
        };

        let dim = serving.resident.spec().dims;
        let model = serving.model.clone();

        {
            let mut active = locked(&self.active);
            let mut cache = locked(&self.cache);

            if cache.active() != Some(model.as_str()) {
                cache.activate(&model)?;
            }

            *active = Some(serving);
        }

        self.events.emit(Event::Ready { model, dim });
        self.remove_leftovers();
        Ok(())
    }

    fn download(&self, manifest: &Manifest) -> Result<std::path::PathBuf, String> {
        model::ensure(
            &self.opts.root,
            manifest,
            self.opts.base_url.as_deref(),
            self.events.as_ref(),
            &|| self.stop.is_stopped(),
        )
        .map_err(|error| error.to_string())
    }

    /// An engine for model `id` in `dir`, loaded.
    fn load(&self, id: &str, dir: std::path::PathBuf, spec: super::manifest::Spec) -> Result<Active, String> {
        let mut resident = Resident::new(Arc::clone(&self.loader), dir, spec, Arc::clone(&self.events));
        resident.ensure()?;

        Ok(Active {
            model: id.to_owned(),
            resident,
        })
    }

    /// Deletes the vectors and folders of models that are neither active nor the manifest's: what
    /// a switch that was killed during its cleanup left behind.
    fn remove_leftovers(&self) {
        let keep = [
            self.active_model().unwrap_or_default(),
            self.opts.manifest.id.to_owned(),
        ];
        let rows = locked(&self.cache).models().unwrap_or_default();

        for stale in rows.iter().filter(|id| !keep.contains(id)) {
            let _ = locked(&self.cache).delete_model(stale);
        }

        for stale in model::folders(&self.opts.root).iter().filter(|id| !keep.contains(id)) {
            let _ = model::remove(&self.opts.root, stale);
        }
    }

    /// Switches to the manifest's model when another one is active. Ok(true) when it switched.
    pub fn update(&self) -> Result<bool, String> {
        let target = self.opts.manifest;

        let Some(from) = self.active_model() else {
            return Err("the model is not ready".to_owned());
        };

        if from == target.id {
            *locked(&self.pending) = None;
            return Ok(false);
        }

        *locked(&self.pending) = Some(target.id.to_owned());
        self.events.emit(Event::ModelUpdate {
            from: from.clone(),
            to: target.id.to_owned(),
        });

        let outcome = self.switch_to(&target, &from);

        if outcome.is_err() && self.active_model().as_deref() != Some(target.id) {
            *locked(&self.pending) = Some(target.id.to_owned());
        }

        outcome.map(|()| true)
    }

    fn switch_to(&self, target: &Manifest, from: &str) -> Result<(), String> {
        let dir = self.download(target)?;
        self.step(Step::Downloaded)?;

        let mut fresh = Resident::new(Arc::clone(&self.loader), dir, target.spec(), Arc::clone(&self.events));
        fresh.ensure()?;
        self.step(Step::Loaded)?;

        // Twice: the second pass catches the notes saved during the first.
        for _ in 0..2 {
            self.reembed(target, &mut fresh)?;
        }

        self.step(Step::Reembedded)?;

        {
            let mut active = locked(&self.active);
            let mut cache = locked(&self.cache);

            cache.activate(target.id)?;
            *active = Some(Active {
                model: target.id.to_owned(),
                resident: fresh,
            });
            *locked(&self.pending) = None;
        }

        self.events.emit(Event::Switched {
            model: target.id.to_owned(),
        });
        self.step(Step::Switched)?;

        locked(&self.cache).delete_model(from)?;
        self.step(Step::RowsDeleted)?;

        model::remove(&self.opts.root, from)
            .map_err(|error| format!("cannot delete the old model: {}", error.kind()))?;
        self.step(Step::FilesDeleted)?;

        self.events.emit(Event::CleanupDone);
        Ok(())
    }

    /// Embeds every note of every store under the new model with `fresh`.
    fn reembed(&self, target: &Manifest, fresh: &mut Resident) -> Result<(), String> {
        for source in &self.sources {
            let mut source = locked(source);

            if source.waiting() {
                continue;
            }

            let total = source.store.note_ids().map_err(|error| error.to_string())?.len();
            let mut embed = |text: &str| -> Result<Vec<f32>, Halt> {
                self.check_step(Step::Reembed)?;
                fresh.embed(text).map_err(Halt::Failed)
            };

            match self.scan(&mut source, Report::Reembed, target.id, 0, total, &mut embed) {
                Ok(_) => {}
                Err(Halt::Failed(message)) => return Err(message),
                Err(Halt::Stopped | Halt::Changed) => return Err("stopped".to_owned()),
            }
        }

        Ok(())
    }

    fn step(&self, step: Step) -> Result<(), String> {
        match self.check_step(step) {
            Ok(()) => Ok(()),
            Err(_) => Err("stopped".to_owned()),
        }
    }

    fn check_step(&self, step: Step) -> Result<(), Halt> {
        self.check_stop()?;

        if locked(&self.checkpoint).as_ref().is_some_and(|hook| !hook(step)) {
            return Err(Halt::Stopped);
        }

        Ok(())
    }

    /// Drops the engine after the idle time; emits `idle` when it did.
    pub fn idle_loop(&self) {
        let pause = (self.opts.idle / 2).clamp(
            std::time::Duration::from_millis(100),
            std::time::Duration::from_secs(30),
        );

        while !self.stop.wait(pause) {
            let unloaded = locked(&self.active)
                .as_mut()
                .is_some_and(|active| active.resident.unload_if_idle(self.opts.idle));

            if unloaded {
                self.events.emit(Event::Idle);
                nudge_allocator();
            }
        }
    }

    /// The life of the embedder after its socket is up: get a model ready (trying again every
    /// poll interval), start the backfill and the idle unload, and switch models when the manifest
    /// asks for it (trying again every retry interval after a failure). Returns when stopped.
    pub fn lifecycle(self: &Arc<Self>) {
        loop {
            match self.prepare() {
                Ok(()) => break,
                Err(message) => {
                    self.events.emit(Event::Error { message });

                    if self.stop.wait(self.opts.poll) {
                        return;
                    }
                }
            }
        }

        let idle = Arc::clone(self);
        thread::spawn(move || idle.idle_loop());

        let backfill = Arc::clone(self);
        thread::spawn(move || backfill.backfill_loop());

        loop {
            match self.update() {
                Ok(_) => return,
                Err(message) => {
                    if self.stop.is_stopped() {
                        return;
                    }

                    self.events.emit(Event::Error { message });

                    if self.stop.wait(self.opts.retry) {
                        return;
                    }
                }
            }
        }
    }
}

/// Gives the allocator the activity it needs to hand freed memory back. mimalloc purges what was
/// freed after a short delay (10 ms) but only when the process allocates again, and an idle
/// embedder does not: so after the model is dropped, a few large allocations spread over a
/// second let the purge run.
fn nudge_allocator() {
    for _ in 0..10 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        std::hint::black_box(vec![0_u8; 1 << 20]);
    }
}
