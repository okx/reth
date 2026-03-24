use reth_primitives_traits::{NodePrimitives, RecoveredBlock};
use std::{any::Any, fmt, sync::Arc};

/// Type-erased, thread-safe extension data attached to a [`PendingFlashBlock`].
///
/// Downstream consumers can downcast to the expected concrete type via
/// [`FlashBlockExtension::downcast_ref`].
///
/// [`PendingFlashBlock`]: crate::PendingFlashBlock
#[derive(Clone)]
pub struct FlashBlockExtension(Arc<dyn Any + Send + Sync>);

impl FlashBlockExtension {
    /// Wraps an arbitrary `Send + Sync` value into a [`FlashBlockExtension`].
    pub fn new<T: Any + Send + Sync>(data: T) -> Self {
        Self(Arc::new(data))
    }

    /// Attempts to downcast the inner value to a concrete type `T`.
    pub fn downcast_ref<T: Any>(&self) -> Option<&T> {
        self.0.downcast_ref()
    }
}

impl fmt::Debug for FlashBlockExtension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FlashBlockExtension").finish_non_exhaustive()
    }
}

/// Hook called after a flashblock is executed in the worker.
///
/// Implementations receive the fully-executed block and can produce an optional
/// [`FlashBlockExtension`] that will be stored on the resulting
/// [`PendingFlashBlock`](crate::PendingFlashBlock).
///
/// This runs inside `spawn_blocking`, so implementations may perform
/// CPU-intensive work (e.g., replaying with an inspector) without blocking the
/// async runtime.
pub trait PostExecutionHook<N: NodePrimitives>: Send + Sync {
    /// Called after a flashblock has been executed.
    ///
    /// `block` is the fully-assembled block with recovered senders.
    /// Return `Some(extension)` to attach data to the pending flashblock,
    /// or `None` to skip.
    fn on_executed(&self, block: &RecoveredBlock<N::Block>) -> Option<FlashBlockExtension>;
}
