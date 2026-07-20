//! Support-fragment selection driven by native methods.
//!
//! The generated JNI source file includes small runtime fragments only when a
//! rendered native method needs them. Methods can require status checks,
//! continuation helpers, exception paths, byte-array helpers, direct-record
//! array helpers, or callback-handle parameters or returns.
//!
//! This module reads the finished method views and records those requirements.
//! It does not inspect binding IR or decide method support; it only prevents the
//! root template from printing unused support code.

use crate::bridge::jni::template::method::NativeMethodView;

pub struct MethodFeatures {
    pub checks_status: bool,
    pub checks_error_buffer: bool,
    pub uses_continuations: bool,
    pub returns_byte_arrays: bool,
    pub uses_record_arrays: bool,
    pub uses_direct_buffers: bool,
    pub uses_exceptions: bool,
    pub returns_callback_handles: bool,
    pub accepts_callback_handles: bool,
}

impl MethodFeatures {
    pub fn from_methods(methods: &[NativeMethodView]) -> Self {
        Self {
            checks_status: methods
                .iter()
                .any(|method| method.checks_status || method.checks_completion_status),
            checks_error_buffer: methods.iter().any(|method| method.checks_error_buffer),
            uses_continuations: methods.iter().any(|method| method.uses_continuations),
            returns_byte_arrays: methods.iter().any(|method| method.returns_bytes),
            uses_record_arrays: methods.iter().any(|method| method.returns_record),
            uses_direct_buffers: methods.iter().any(|method| {
                !method.direct_buffers.is_empty() || !method.record_buffers.is_empty()
            }),
            uses_exceptions: methods.iter().any(|method| {
                method.checks_status
                    || method.checks_completion_status
                    || method.checks_error_buffer
                    || method.returns_bytes
                    || method.returns_record
                    || method.returns_callback
                    || !method.borrowed_arrays.is_empty()
                    || !method.direct_buffers.is_empty()
                    || !method.record_buffers.is_empty()
            }),
            returns_callback_handles: methods.iter().any(|method| method.returns_callback),
            accepts_callback_handles: methods.iter().any(|method| {
                method
                    .parameters
                    .iter()
                    .any(|parameter| parameter.is_callback)
            }),
        }
    }
}
