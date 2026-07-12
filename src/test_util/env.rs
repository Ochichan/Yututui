//! Edition 2024: `set_var`/`remove_var` are unsafe. Quarantine all test env
//! mutation here behind a process-global lock.
//!
//! Prefer [`with_var`] / [`with_vars`]. Do not call [`set_var`] / [`remove_var`] /
//! [`EnvRestore::capture`] from inside a `with_*` closure unless you rely on the
//! reentrant path below (same-thread nesting is supported; cross-thread still serializes).

use std::cell::Cell;
use std::ffi::{OsStr, OsString};
use std::sync::{Mutex, MutexGuard};

thread_local! {
    static LOCK_DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// Whether this thread is inside a [`with_var`] / [`with_vars`] scope which owns the
/// process-global environment lock. Path resolution uses this in unit tests so an override is
/// visible to the test which installed it without leaking into unrelated parallel readers.
pub(crate) fn scoped_mutation_active() -> bool {
    LOCK_DEPTH.with(|depth| depth.get() > 0)
}

struct DepthGuard;

impl Drop for DepthGuard {
    fn drop(&mut self) {
        LOCK_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

/// Process-global env mutex. Prefer [`with_vars`] over holding this across custom logic.
pub fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn enter_locked(guard: MutexGuard<'static, ()>) -> (MutexGuard<'static, ()>, DepthGuard) {
    LOCK_DEPTH.with(|depth| depth.set(depth.get() + 1));
    (guard, DepthGuard)
}

fn acquire_lock() -> Option<(MutexGuard<'static, ()>, DepthGuard)> {
    if LOCK_DEPTH.with(|depth| depth.get() > 0) {
        // Already held on this thread (e.g. inside `with_vars`); skip re-lock.
        None
    } else {
        Some(enter_locked(env_lock()))
    }
}

pub fn set_var<K, V>(name: K, value: V)
where
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    let _owned = acquire_lock();
    // SAFETY: process-global env mutation; serialized by env_lock / same-thread depth; test-only.
    unsafe { std::env::set_var(name, value) };
}

pub fn remove_var<K>(name: K)
where
    K: AsRef<OsStr>,
{
    let _owned = acquire_lock();
    // SAFETY: process-global env mutation; serialized by env_lock / same-thread depth; test-only.
    unsafe { std::env::remove_var(name) };
}

pub struct EnvRestore {
    values: Vec<(String, Option<OsString>)>,
}

impl EnvRestore {
    pub fn capture(names: &[&str]) -> Self {
        let _owned = acquire_lock();
        Self {
            values: capture_env(names),
        }
    }
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        let _owned = acquire_lock();
        restore_env(&self.values);
    }
}

pub fn with_var<T>(name: &str, value: Option<&str>, f: impl FnOnce() -> T) -> T {
    with_vars(&[(name, value)], f)
}

pub fn with_vars<T>(vars: &[(&str, Option<&str>)], f: impl FnOnce() -> T) -> T {
    let (_guard, _depth) = enter_locked(env_lock());
    let names: Vec<&str> = vars.iter().map(|(name, _)| *name).collect();
    let restore = ScopedEnvRestore {
        values: capture_env(&names),
    };
    for (name, value) in vars {
        match value {
            Some(value) => set_var_unlocked(name, value),
            None => remove_var_unlocked(name),
        }
    }
    let result = f();
    drop(restore);
    result
}

struct ScopedEnvRestore {
    values: Vec<(String, Option<OsString>)>,
}

impl Drop for ScopedEnvRestore {
    fn drop(&mut self) {
        restore_env(&self.values);
    }
}

fn capture_env(names: &[&str]) -> Vec<(String, Option<OsString>)> {
    let mut values = Vec::new();
    for name in names {
        if values
            .iter()
            .any(|(captured, _): &(String, Option<OsString>)| captured == name)
        {
            continue;
        }
        values.push(((*name).to_owned(), std::env::var_os(name)));
    }
    values
}

fn restore_env(values: &[(String, Option<OsString>)]) {
    for (name, value) in values {
        match value {
            Some(value) => set_var_unlocked(name, value),
            None => remove_var_unlocked(name),
        }
    }
}

fn set_var_unlocked<K, V>(name: K, value: V)
where
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    // SAFETY: caller holds env_lock (or same-thread depth); test-only module.
    unsafe { std::env::set_var(name, value) };
}

fn remove_var_unlocked<K>(name: K)
where
    K: AsRef<OsStr>,
{
    // SAFETY: caller holds env_lock (or same-thread depth); test-only module.
    unsafe { std::env::remove_var(name) };
}
