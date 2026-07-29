// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Customation AS
//! Raw ABI of libgnubgapi (gnubgapi/native/gnubgapi.h) plus a runtime
//! loader. Struct layouts mirror the header field for field.
//!
//! gnubg's evaluation core is globally stateful (static caches, one
//! neural net), so ONE context exists per process and every call on it
//! is serialized by the engine layer.

use std::ffi::{c_char, c_int, CStr, CString};
use std::path::Path;

use libloading::{Library, Symbol};

pub const GNUBGAPI_OK: c_int = 0;

pub const NUM_OUTPUTS: usize = 7;
pub const MAX_MOVES: usize = 3060;
pub const MOVE_STEPS: usize = 8;
const RESULT_POSITION_ID_LEN: usize = 16;

/// Output layout of the 7-double evaluation array.
pub mod outputs {
    pub const WIN: usize = 0;
    pub const WIN_GAMMON: usize = 1;
    pub const WIN_BACKGAMMON: usize = 2;
    pub const LOSE_GAMMON: usize = 3;
    pub const LOSE_BACKGAMMON: usize = 4;
    pub const CUBELESS: usize = 5;
    pub const CUBEFUL: usize = 6;
}

/// gnubg's cubedecision values (stable wire constants from gnubgapi.h).
pub mod cube_decision {
    pub const DOUBLE_TAKE: i32 = 0;
    pub const DOUBLE_PASS: i32 = 1;
    pub const NODOUBLE_TAKE: i32 = 2;
    pub const TOOGOOD_TAKE: i32 = 3;
    pub const TOOGOOD_PASS: i32 = 4;
    pub const DOUBLE_BEAVER: i32 = 5;
    pub const NODOUBLE_BEAVER: i32 = 6;
    pub const REDOUBLE_TAKE: i32 = 7;
    pub const REDOUBLE_PASS: i32 = 8;
    pub const NO_REDOUBLE_TAKE: i32 = 9;
    pub const TOOGOODRE_TAKE: i32 = 10;
    pub const TOOGOODRE_PASS: i32 = 11;
    pub const NO_REDOUBLE_BEAVER: i32 = 12;
    pub const NODOUBLE_DEADCUBE: i32 = 13;
    pub const NO_REDOUBLE_DEADCUBE: i32 = 14;
    pub const NOT_AVAILABLE: i32 = 15;
    pub const OPTIONAL_DOUBLE_TAKE: i32 = 16;
    pub const OPTIONAL_REDOUBLE_TAKE: i32 = 17;
    pub const OPTIONAL_DOUBLE_BEAVER: i32 = 18;
    pub const OPTIONAL_DOUBLE_PASS: i32 = 19;
    pub const OPTIONAL_REDOUBLE_PASS: i32 = 20;
}

/// Indices into `CubeDecisionResult.equities`.
pub mod equity_index {
    pub const NODOUBLE: usize = 1;
    pub const TAKE: usize = 2;
    pub const DROP: usize = 3;
}

/// Mirrors `gnubgapi_rollout_settings`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RolloutSettings {
    pub n_trials: u32,
    pub cubeful: i32,
    pub variance_reduction: i32,
    pub chequer_plies: u32,
    pub cube_plies: u32,
    pub seed: u32,
    pub truncate: i32,
    pub truncate_plies: u32,
}

/// Mirrors `gnubgapi_move`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct NativeMove {
    pub an_move: [c_int; MOVE_STEPS],
    pub result_position_id: [c_char; RESULT_POSITION_ID_LEN],
    pub n_submoves: u32,
    pub pips: u32,
}

/// Mirrors `gnubgapi_scored_move`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ScoredMove {
    pub mv: NativeMove,
    pub equity: f64,
    pub probs: [f64; 5],
}

impl ScoredMove {
    pub fn zeroed() -> ScoredMove {
        unsafe { std::mem::zeroed() }
    }
}

/// Mirrors `gnubgapi_cube_decision_result`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CubeDecisionResult {
    pub cubeful_outputs: [[f64; NUM_OUTPUTS]; 2],
    pub equities: [f64; 4],
    pub decision: i32,
}

pub enum ContextOpaque {}

type FnGetVersion = unsafe extern "C" fn(*mut u32, *mut u32, *mut u32);
type FnGetLastError = unsafe extern "C" fn() -> *const c_char;
type FnCreate = unsafe extern "C" fn() -> *mut ContextOpaque;
type FnDestroy = unsafe extern "C" fn(*mut ContextOpaque);
type FnInit = unsafe extern "C" fn(
    *mut ContextOpaque,
    *const c_char,
    *const c_char,
    *const c_char,
    c_int,
) -> c_int;
type FnShutdown = unsafe extern "C" fn(*mut ContextOpaque);
type FnEvaluateFullPlied = unsafe extern "C" fn(
    *mut ContextOpaque,
    *const c_char,
    *const c_char,
    u32,
    *mut f64,
) -> c_int;
type FnRolloutDefaults = unsafe extern "C" fn(*mut RolloutSettings);
type FnRollout = unsafe extern "C" fn(
    *mut ContextOpaque,
    *const c_char,
    *const c_char,
    *const RolloutSettings,
    *mut f64,
    *mut f64,
) -> c_int;
type FnGenerateMovesWithEval = unsafe extern "C" fn(
    *mut ContextOpaque,
    *const c_char,
    *const c_char,
    c_int,
    c_int,
    u32,
    *mut ScoredMove,
    *mut u32,
) -> c_int;
type FnEvaluateCubeDecision = unsafe extern "C" fn(
    *mut ContextOpaque,
    *const c_char,
    *const c_char,
    u32,
    *mut CubeDecisionResult,
) -> c_int;

#[derive(Debug, Clone)]
pub struct GnubgApiError {
    pub code: c_int,
    pub message: String,
}

impl std::fmt::Display for GnubgApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "gnubgapi error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for GnubgApiError {}

pub struct GnubgApi {
    lib: Library,
}

// SAFETY: call serialization on the single context is enforced by the
// engine layer's global lock.
unsafe impl Send for GnubgApi {}
unsafe impl Sync for GnubgApi {}

impl GnubgApi {
    pub fn load(path: &Path) -> Result<Self, libloading::Error> {
        unsafe { Library::new(path).map(|lib| GnubgApi { lib }) }
    }

    fn sym<'a, T>(&'a self, name: &[u8]) -> Symbol<'a, T> {
        unsafe {
            self.lib.get(name).unwrap_or_else(|e| {
                panic!(
                    "gnubgapi library is missing symbol {}: {e}",
                    String::from_utf8_lossy(name)
                )
            })
        }
    }

    fn last_error(&self, code: c_int) -> GnubgApiError {
        let f: Symbol<FnGetLastError> = self.sym(b"gnubgapi_get_last_error");
        let message = unsafe {
            let ptr = f();
            if ptr.is_null() {
                String::new()
            } else {
                CStr::from_ptr(ptr).to_string_lossy().into_owned()
            }
        };
        GnubgApiError { code, message }
    }

    pub fn version(&self) -> String {
        let f: Symbol<FnGetVersion> = self.sym(b"gnubgapi_get_version");
        let (mut major, mut minor, mut patch) = (0u32, 0u32, 0u32);
        unsafe { f(&mut major, &mut minor, &mut patch) };
        format!("{major}.{minor}.{patch}")
    }

    pub fn create(&self) -> Result<*mut ContextOpaque, GnubgApiError> {
        let f: Symbol<FnCreate> = self.sym(b"gnubgapi_create");
        let ctx = unsafe { f() };
        if ctx.is_null() {
            Err(self.last_error(GNUBGAPI_OK))
        } else {
            Ok(ctx)
        }
    }

    /// SAFETY: caller serializes calls on `ctx`.
    pub unsafe fn init(
        &self,
        ctx: *mut ContextOpaque,
        weights_path: &CString,
        weights_binary_path: &CString,
        data_dir: &CString,
        no_bearoff: bool,
    ) -> Result<(), GnubgApiError> {
        let f: Symbol<FnInit> = self.sym(b"gnubgapi_init");
        let code = f(
            ctx,
            weights_path.as_ptr(),
            weights_binary_path.as_ptr(),
            data_dir.as_ptr(),
            no_bearoff as c_int,
        );
        if code == GNUBGAPI_OK {
            Ok(())
        } else {
            Err(self.last_error(code))
        }
    }

    /// SAFETY: caller serializes calls on `ctx`; no calls in flight.
    pub unsafe fn shutdown_and_destroy(&self, ctx: *mut ContextOpaque) {
        let shutdown: Symbol<FnShutdown> = self.sym(b"gnubgapi_shutdown");
        shutdown(ctx);
        let destroy: Symbol<FnDestroy> = self.sym(b"gnubgapi_destroy");
        destroy(ctx);
    }

    /// SAFETY: caller serializes calls on `ctx`.
    pub unsafe fn evaluate_position_full_plied(
        &self,
        ctx: *mut ContextOpaque,
        position_id: &CString,
        match_id: &CString,
        n_plies: u32,
    ) -> Result<[f64; NUM_OUTPUTS], GnubgApiError> {
        let f: Symbol<FnEvaluateFullPlied> = self.sym(b"gnubgapi_evaluate_position_full_plied");
        let mut output = [0f64; NUM_OUTPUTS];
        let code = f(ctx, position_id.as_ptr(), match_id.as_ptr(), n_plies, output.as_mut_ptr());
        if code == GNUBGAPI_OK {
            Ok(output)
        } else {
            Err(self.last_error(code))
        }
    }

    pub fn rollout_settings_defaults(&self) -> RolloutSettings {
        let f: Symbol<FnRolloutDefaults> = self.sym(b"gnubgapi_rollout_settings_default");
        let mut settings = std::mem::MaybeUninit::<RolloutSettings>::uninit();
        unsafe {
            f(settings.as_mut_ptr());
            settings.assume_init()
        }
    }

    /// SAFETY: caller serializes calls on `ctx`.
    pub unsafe fn rollout_position(
        &self,
        ctx: *mut ContextOpaque,
        position_id: &CString,
        match_id: &CString,
        settings: &RolloutSettings,
    ) -> Result<[f64; NUM_OUTPUTS], GnubgApiError> {
        let f: Symbol<FnRollout> = self.sym(b"gnubgapi_rollout_position");
        let mut output = [0f64; NUM_OUTPUTS];
        let mut std_dev = [0f64; NUM_OUTPUTS];
        let code = f(
            ctx,
            position_id.as_ptr(),
            match_id.as_ptr(),
            settings,
            output.as_mut_ptr(),
            std_dev.as_mut_ptr(),
        );
        if code == GNUBGAPI_OK {
            Ok(output)
        } else {
            Err(self.last_error(code))
        }
    }

    /// SAFETY: caller serializes calls on `ctx`; `buffer` must hold
    /// MAX_MOVES entries (the ABI's contract).
    pub unsafe fn generate_moves_with_eval(
        &self,
        ctx: *mut ContextOpaque,
        position_id: &CString,
        match_id: &CString,
        die1: i32,
        die2: i32,
        n_plies: u32,
        buffer: &mut [ScoredMove],
    ) -> Result<usize, GnubgApiError> {
        assert!(buffer.len() >= MAX_MOVES, "gnubgapi requires a MAX_MOVES buffer");
        let f: Symbol<FnGenerateMovesWithEval> = self.sym(b"gnubgapi_generate_moves_with_eval");
        let mut count: u32 = 0;
        let code = f(
            ctx,
            position_id.as_ptr(),
            match_id.as_ptr(),
            die1,
            die2,
            n_plies,
            buffer.as_mut_ptr(),
            &mut count,
        );
        if code == GNUBGAPI_OK {
            Ok(count as usize)
        } else {
            Err(self.last_error(code))
        }
    }

    /// SAFETY: caller serializes calls on `ctx`.
    pub unsafe fn evaluate_cube_decision(
        &self,
        ctx: *mut ContextOpaque,
        position_id: &CString,
        match_id: &CString,
        n_plies: u32,
    ) -> Result<CubeDecisionResult, GnubgApiError> {
        let f: Symbol<FnEvaluateCubeDecision> = self.sym(b"gnubgapi_evaluate_cube_decision");
        let mut out = CubeDecisionResult::default();
        let code = f(ctx, position_id.as_ptr(), match_id.as_ptr(), n_plies, &mut out);
        if code == GNUBGAPI_OK {
            Ok(out)
        } else {
            Err(self.last_error(code))
        }
    }
}
