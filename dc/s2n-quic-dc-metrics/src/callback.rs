// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::any::Any;

use crate::{
    backend::{Backend, CallbackValue, MetricInfo},
    Unit,
};

/// This is a helper trait that is used for the `Registry::register_list_callback` method, which
/// allows registering callbacks for a particular metric to be called when needed.
pub(crate) trait ValueList {
    /// Invokes the callbacks and reports their values to `backend` as a single logical metric.
    fn report(&mut self, info: &MetricInfo<'_>, backend: &mut dyn Backend);

    /// Whether this metric suppresses a zero value (e.g. a gauge).
    fn zero_suppressed(&self) -> bool;

    /// The display unit for this metric.
    fn unit(&self) -> Unit;

    /// Returns any for the self type, used to cast back when registering equally-named metrics.
    fn as_any(&mut self) -> &mut dyn Any;
}

/// The storage backing a callback-list metric: the callbacks, the display unit, and whether a
/// zero value should be suppressed from output.
pub(crate) struct CallbackList<F> {
    pub(crate) callbacks: Vec<F>,
    pub(crate) unit: Unit,
    pub(crate) zero_suppressed: bool,
}

impl<V, F> ValueList for CallbackList<F>
where
    F: FnMut() -> V + Send + 'static,
    V: CallbackValue,
{
    fn report(&mut self, info: &MetricInfo<'_>, backend: &mut dyn Backend) {
        // Invoke every callback once (they may be stateful, e.g. delta counters) and materialize the
        // values so we can hand the backend a slice of trait objects.
        let values: Vec<V> = self.callbacks.iter_mut().map(|cb| cb()).collect();
        let refs: Vec<&dyn CallbackValue> = values.iter().map(|v| v as &dyn CallbackValue).collect();
        backend.record_callback(info, &refs);
    }

    fn zero_suppressed(&self) -> bool {
        self.zero_suppressed
    }

    fn unit(&self) -> Unit {
        self.unit
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}
