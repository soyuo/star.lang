//! Guest-visible cooperative channels and mutexes.

use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::mem::size_of;

use num_traits::ToPrimitive;
use star_parser::Span;

use super::{
    estimated_unmanaged_payload_bytes, projected_capacity_after_push, GuestChannel, GuestMutex,
    Interpreter, RuntimeError, RuntimeErrorKind,
};
use crate::value::Value;

impl<R: BufRead, W: Write> Interpreter<R, W> {
    pub(super) fn create_channel(&mut self, span: Span) -> Result<(), RuntimeError> {
        let value = self.pop(span)?;
        let Value::Int(capacity) = value else {
            return Err(self.type_error("nonnegative Int channel capacity", &value, span));
        };
        let capacity = capacity
            .to_usize()
            .ok_or_else(|| self.error(RuntimeErrorKind::ResourceLimit("channel capacity"), span))?;
        let next_id = self.next_channel_id.checked_add(1).ok_or_else(|| {
            self.error(RuntimeErrorKind::ResourceLimit("channel identifiers"), span)
        })?;
        let target_capacity =
            projected_capacity_after_push(self.channels.len(), self.channels.capacity());
        let queue_capacity = capacity.max(1);
        let additional = size_of::<(u64, GuestChannel)>()
            .saturating_add(
                target_capacity
                    .saturating_sub(self.channels.capacity())
                    .saturating_mul(size_of::<(u64, GuestChannel)>()),
            )
            .saturating_add(queue_capacity.saturating_mul(size_of::<Value>()));
        self.ensure_memory_budget(additional, span)?;
        let mut queue = VecDeque::new();
        queue
            .try_reserve_exact(queue_capacity)
            .map_err(|_| self.error(RuntimeErrorKind::ResourceLimit("channel capacity"), span))?;
        self.channels
            .try_reserve(1)
            .map_err(|_| self.error(RuntimeErrorKind::ResourceLimit("channel registry"), span))?;
        let id = self.next_channel_id;
        self.next_channel_id = next_id;
        self.channels.insert(
            id,
            GuestChannel {
                capacity,
                queue,
                waiting_receivers: 0,
            },
        );
        self.push_value(Value::Channel(id), span)
    }

    pub(super) fn account_pending_channel_value(
        &mut self,
        value: &Value,
        span: Span,
    ) -> Result<usize, RuntimeError> {
        let bytes = size_of::<Value>().saturating_add(estimated_unmanaged_payload_bytes(value, 0));
        self.pending_channel_message_bytes =
            self.pending_channel_message_bytes.saturating_add(bytes);
        if let Err(error) = self.ensure_memory_budget(0, span) {
            self.pending_channel_message_bytes =
                self.pending_channel_message_bytes.saturating_sub(bytes);
            return Err(error);
        }
        Ok(bytes)
    }

    pub(super) fn release_pending_channel_value(&mut self, bytes: usize) {
        self.pending_channel_message_bytes =
            self.pending_channel_message_bytes.saturating_sub(bytes);
    }

    pub(super) fn check_async_context(&self, span: Span) -> Result<u64, RuntimeError> {
        self.current_task_id.ok_or_else(|| {
            self.error(
                RuntimeErrorKind::TypeError {
                    expected: "async task context",
                    found: "main execution context",
                },
                span,
            )
        })
    }

    pub(super) fn try_send_channel(
        &mut self,
        channel_id: u64,
        value: &Value,
        span: Span,
    ) -> Result<bool, RuntimeError> {
        let Some(state) = self.channels.get(&channel_id) else {
            return Err(self.error(
                RuntimeErrorKind::ReferenceError("channel does not exist"),
                span,
            ));
        };
        let rendezvous_ready =
            state.capacity == 0 && state.waiting_receivers > 0 && state.queue.is_empty();
        if state.queue.len() >= state.capacity && !rendezvous_ready {
            return Ok(false);
        }
        self.ensure_memory_budget(estimated_unmanaged_payload_bytes(value, 0), span)?;
        self.channels
            .get_mut(&channel_id)
            .expect("channel was checked above")
            .queue
            .push_back(value.clone());
        Ok(true)
    }

    pub(super) fn try_receive_channel(
        &mut self,
        channel_id: u64,
        span: Span,
    ) -> Result<Option<Value>, RuntimeError> {
        let Some(state) = self.channels.get_mut(&channel_id) else {
            return Err(self.error(
                RuntimeErrorKind::ReferenceError("channel does not exist"),
                span,
            ));
        };
        Ok(state.queue.pop_front())
    }

    pub(super) fn register_channel_receiver(
        &mut self,
        channel_id: u64,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let Some(state) = self.channels.get_mut(&channel_id) else {
            return Err(self.error(
                RuntimeErrorKind::ReferenceError("channel does not exist"),
                span,
            ));
        };
        state.waiting_receivers = state.waiting_receivers.saturating_add(1);
        Ok(())
    }

    pub(super) fn unregister_channel_receiver(&mut self, channel_id: u64) {
        if let Some(state) = self.channels.get_mut(&channel_id) {
            state.waiting_receivers = state.waiting_receivers.saturating_sub(1);
        }
    }

    pub(super) fn create_mutex(&mut self, span: Span) -> Result<(), RuntimeError> {
        let next_id = self.next_mutex_id.checked_add(1).ok_or_else(|| {
            self.error(RuntimeErrorKind::ResourceLimit("mutex identifiers"), span)
        })?;
        let target_capacity =
            projected_capacity_after_push(self.mutexes.len(), self.mutexes.capacity());
        let additional = size_of::<(u64, GuestMutex)>().saturating_add(
            target_capacity
                .saturating_sub(self.mutexes.capacity())
                .saturating_mul(size_of::<(u64, GuestMutex)>()),
        );
        self.ensure_memory_budget(additional, span)?;
        self.mutexes
            .try_reserve(1)
            .map_err(|_| self.error(RuntimeErrorKind::ResourceLimit("mutex registry"), span))?;
        let id = self.next_mutex_id;
        self.next_mutex_id = next_id;
        self.mutexes.insert(id, GuestMutex::default());
        self.push_value(Value::Mutex(id), span)
    }

    pub(super) fn try_lock_mutex(
        &mut self,
        mutex_id: u64,
        task_id: u64,
        span: Span,
    ) -> Result<bool, RuntimeError> {
        let Some(state) = self.mutexes.get_mut(&mutex_id) else {
            return Err(self.error(
                RuntimeErrorKind::ReferenceError("mutex does not exist"),
                span,
            ));
        };
        match state.owner {
            None => {
                state.owner = Some(task_id);
                Ok(true)
            }
            Some(owner) if owner == task_id => Err(self.error(
                RuntimeErrorKind::TypeError {
                    expected: "unlocked non-reentrant Mutex",
                    found: "Mutex",
                },
                span,
            )),
            Some(_) => Ok(false),
        }
    }

    pub(super) fn unlock_mutex_for_task(
        &mut self,
        mutex_id: u64,
        task_id: u64,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let Some(state) = self.mutexes.get_mut(&mutex_id) else {
            return Err(self.error(
                RuntimeErrorKind::ReferenceError("mutex does not exist"),
                span,
            ));
        };
        if state.owner != Some(task_id) {
            return Err(self.error(RuntimeErrorKind::PermissionError("mutex ownership"), span));
        }
        state.owner = None;
        Ok(())
    }

    pub(super) fn send_channel(&mut self, span: Span) -> Result<(), RuntimeError> {
        let value = self.pop(span)?;
        let channel = self.pop(span)?;
        let Value::Channel(channel_id) = channel else {
            return Err(self.type_error("Channel", &channel, span));
        };
        self.check_async_context(span)?;
        let pending = self.account_pending_channel_value(&value, span)?;
        let result = loop {
            match self.try_send_channel(channel_id, &value, span) {
                Ok(true) => break Ok(()),
                Ok(false) => match self.run_next_task(span) {
                    Ok(true) => {
                        if let Err(error) = self.check_resource_limits(span) {
                            break Err(error);
                        }
                    }
                    Ok(false) => break Err(self.error(RuntimeErrorKind::Deadlock, span)),
                    Err(error) => break Err(error),
                },
                Err(error) => break Err(error),
            }
        };
        self.release_pending_channel_value(pending);
        result?;
        self.push_value(Value::Null, span)
    }

    pub(super) fn receive_channel(&mut self, span: Span) -> Result<(), RuntimeError> {
        let channel = self.pop(span)?;
        let Value::Channel(channel_id) = channel else {
            return Err(self.type_error("Channel", &channel, span));
        };
        self.check_async_context(span)?;
        let mut waiting = false;
        let result = loop {
            match self.try_receive_channel(channel_id, span) {
                Ok(Some(value)) => {
                    if waiting {
                        self.unregister_channel_receiver(channel_id);
                    }
                    break Ok(value);
                }
                Ok(None) => {
                    if !waiting {
                        self.register_channel_receiver(channel_id, span)?;
                        waiting = true;
                    }
                    match self.run_next_task(span) {
                        Ok(true) => {
                            if let Err(error) = self.check_resource_limits(span) {
                                self.unregister_channel_receiver(channel_id);
                                break Err(error);
                            }
                        }
                        Ok(false) => {
                            self.unregister_channel_receiver(channel_id);
                            break Err(self.error(RuntimeErrorKind::Deadlock, span));
                        }
                        Err(error) => {
                            self.unregister_channel_receiver(channel_id);
                            break Err(error);
                        }
                    }
                }
                Err(error) => {
                    if waiting {
                        self.unregister_channel_receiver(channel_id);
                    }
                    break Err(error);
                }
            }
        };
        self.push_value(result?, span)
    }

    pub(super) fn acquire_mutex(&mut self, span: Span) -> Result<(), RuntimeError> {
        let mutex = self.pop(span)?;
        let Value::Mutex(mutex_id) = mutex else {
            return Err(self.type_error("Mutex", &mutex, span));
        };
        let task_id = self.check_async_context(span)?;
        loop {
            match self.try_lock_mutex(mutex_id, task_id, span) {
                Ok(true) => break,
                Ok(false) => match self.run_next_task(span) {
                    Ok(true) => self.check_resource_limits(span)?,
                    Ok(false) => return Err(self.error(RuntimeErrorKind::Deadlock, span)),
                    Err(error) => return Err(error),
                },
                Err(error) => return Err(error),
            }
        }
        self.push_value(Value::Null, span)
    }

    pub(super) fn release_mutex(&mut self, span: Span) -> Result<(), RuntimeError> {
        let mutex = self.pop(span)?;
        let Value::Mutex(mutex_id) = mutex else {
            return Err(self.type_error("Mutex", &mutex, span));
        };
        let task_id = self.check_async_context(span)?;
        self.unlock_mutex_for_task(mutex_id, task_id, span)?;
        self.push_value(Value::Null, span)
    }
}
