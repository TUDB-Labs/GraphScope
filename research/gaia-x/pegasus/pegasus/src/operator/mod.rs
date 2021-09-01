//
//! Copyright 2020 Alibaba Group Holding Limited.
//!
//! Licensed under the Apache License, Version 2.0 (the "License");
//! you may not use this file except in compliance with the License.
//! You may obtain a copy of the License at
//!
//! http://www.apache.org/licenses/LICENSE-2.0
//!
//! Unless required by applicable law or agreed to in writing, software
//! distributed under the License is distributed on an "AS IS" BASIS,
//! WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//! See the License for the specific language governing permissions and
//! limitations under the License.

use std::cell::Cell;
use std::time::Instant;

use nohash_hasher::IntSet;

use crate::api::meta::OperatorInfo;
use crate::api::notification::{CancelScope, EndScope};
use crate::channel_id::ChannelInfo;
use crate::communication::input::{new_input, InputProxy};
use crate::communication::output::{OutputBuilder, OutputBuilderImpl, OutputProxy};
use crate::data::MicroBatch;
use crate::data_plane::{GeneralPull, GeneralPush};
use crate::errors::{IOResult, JobExecError};
use crate::event::emitter::EventEmitter;
use crate::graph::Port;
use crate::progress::EndSignal;
use crate::schedule::state::inbound::InputEndNotify;
use crate::schedule::state::outbound::OutputCancelState;
use crate::tag::tools::map::TidyTagMap;
use crate::{Data, Tag};

pub trait Notifiable: Send + 'static {
    fn on_notify(&mut self, n: EndScope, outputs: &[Box<dyn OutputProxy>]) -> Result<(), JobExecError>;

    fn on_cancel(&mut self, n: CancelScope, inputs: &[Box<dyn InputProxy>]) -> Result<(), JobExecError>;
}

impl<T: ?Sized + Notifiable> Notifiable for Box<T> {
    fn on_notify(&mut self, n: EndScope, outputs: &[Box<dyn OutputProxy>]) -> Result<(), JobExecError> {
        (**self).on_notify(n, outputs)
    }

    fn on_cancel(&mut self, n: CancelScope, inputs: &[Box<dyn InputProxy>]) -> Result<(), JobExecError> {
        (**self).on_cancel(n, inputs)
    }
}

struct MultiInputsMerge {
    input_size: usize,
    end_merge: Vec<TidyTagMap<(EndScope, IntSet<u64>)>>,
}

impl MultiInputsMerge {
    pub fn new(input_size: usize, scope_level: u32) -> Self {
        let mut end_merge = Vec::with_capacity(scope_level as usize + 1);
        for i in 0..scope_level + 1 {
            end_merge.push(TidyTagMap::new(i));
        }
        MultiInputsMerge { input_size, end_merge }
    }

    fn merge_end(&mut self, n: EndScope) -> Option<EndScope> {
        let idx = n.tag().len();
        assert!(idx < self.end_merge.len());
        if let Some((mut merged, mut count)) = self.end_merge[idx].remove(n.tag()) {
            let EndScope { port, tag, weight } = n;
            if count.insert(port as u64) {
                trace_worker!("merge {}th end of {:?} from input port {}", count.len(), tag, port);
                merged.weight.merge(weight);
                if count.len() == self.input_size {
                    Some(merged)
                } else {
                    self.end_merge[idx].insert(tag, (merged, count));
                    None
                }
            } else {
                None
            }
        } else {
            trace_worker!("merge first end of {:?} from input port {}", n.tag(), n.port);
            let mut m = IntSet::default();
            m.insert(n.port as u64);
            self.end_merge[idx].insert(n.tag().clone(), (n, m));
            None
        }
    }
}

#[allow(dead_code)]
struct MultiOutputsMerge {
    output_size: usize,
    scope_level: u32,
    cancel_merge: Vec<TidyTagMap<IntSet<u64>>>,
}

impl MultiOutputsMerge {
    fn new(output_size: usize, scope_level: u32) -> MultiOutputsMerge {
        let mut cancel_merge = Vec::with_capacity(scope_level as usize + 1);
        for i in 0..scope_level + 1 {
            cancel_merge.push(TidyTagMap::new(i));
        }
        MultiOutputsMerge { output_size, scope_level, cancel_merge }
    }

    // TODO: enable merge cancel from parent into children;
    fn merge_cancel(&mut self, n: CancelScope) -> Option<Tag> {
        let level = n.tag().len();
        assert!(level < self.cancel_merge.len());
        if let Some(mut in_merge) = self.cancel_merge[level].remove(n.tag()) {
            in_merge.insert(n.port as u64);
            let left = self.output_size - in_merge.len();
            if left == 0 {
                Some(n.tag)
            } else {
                trace_worker!("EARLY_STOP: other {} output still send data of {:?};", left, n.tag);
                self.cancel_merge[level].insert(n.tag().clone(), in_merge);
                None
            }
        } else {
            let mut m = IntSet::default();
            m.insert(n.port as u64);
            self.cancel_merge[level].insert(n.tag().clone(), m);
            None
        }
    }
}

enum DefaultNotify {
    SISO,
    /// Multi-Inputs-Single-Output
    MISO(MultiInputsMerge),
    /// Single-Input-Multi-Outputs
    SIMO(MultiOutputsMerge),
    /// Multi-Inputs-Multi-Outputs
    MIMO(MultiInputsMerge, MultiOutputsMerge),
}

impl DefaultNotify {
    fn new(input_size: usize, output_size: usize, scope_level: u32) -> Self {
        if input_size > 1 {
            let mim = MultiInputsMerge::new(input_size, scope_level);
            if output_size > 1 {
                let mom = MultiOutputsMerge::new(output_size, scope_level);
                DefaultNotify::MIMO(mim, mom)
            } else {
                DefaultNotify::MISO(mim)
            }
        } else if output_size > 1 {
            let mom = MultiOutputsMerge::new(output_size, scope_level);
            DefaultNotify::SIMO(mom)
        } else {
            DefaultNotify::SISO
        }
    }

    fn merge_end(&mut self, end: EndScope) -> Option<EndScope> {
        match self {
            DefaultNotify::SISO | DefaultNotify::SIMO(_) => Some(end),
            DefaultNotify::MISO(mim) => mim.merge_end(end),
            DefaultNotify::MIMO(mim, _) => mim.merge_end(end),
        }
    }

    fn merge_cancel(&mut self, cancel: CancelScope) -> Option<Tag> {
        match self {
            DefaultNotify::SISO | DefaultNotify::MISO(_) => Some(cancel.tag),
            DefaultNotify::SIMO(mom) => mom.merge_cancel(cancel),
            DefaultNotify::MIMO(_, mom) => mom.merge_cancel(cancel),
        }
    }
}

pub struct DefaultNotifyOperator<T> {
    op: T,
    notify: DefaultNotify,
}

impl<T> DefaultNotifyOperator<T> {
    fn new(input_size: usize, output_size: usize, scope_level: u32, op: T) -> Self {
        let notify = DefaultNotify::new(input_size, output_size, scope_level);
        DefaultNotifyOperator { op, notify }
    }
}

impl<T: Send + 'static> Notifiable for DefaultNotifyOperator<T> {
    fn on_notify(&mut self, n: EndScope, outputs: &[Box<dyn OutputProxy>]) -> Result<(), JobExecError> {
        if !outputs.is_empty() {
            if let Some(end) = self.notify.merge_end(n) {
                let EndScope { port: _port, tag, weight } = end;
                let sig = EndSignal::new(tag, weight);
                if outputs.len() > 1 {
                    for i in 1..outputs.len() {
                        outputs[i].notify_end(sig.clone())?;
                    }
                }
                outputs[0].notify_end(sig)?;
            }
            Ok(())
        } else {
            Ok(())
        }
    }

    fn on_cancel(&mut self, n: CancelScope, inputs: &[Box<dyn InputProxy>]) -> Result<(), JobExecError> {
        if !inputs.is_empty() {
            if let Some(cancel) = self.notify.merge_cancel(n) {
                for input in inputs {
                    input.cancel_scope(&cancel);
                }
            }
        }
        Ok(())
    }
}

pub trait OperatorCore: Send + 'static {
    fn on_receive(
        &mut self, inputs: &[Box<dyn InputProxy>], outputs: &[Box<dyn OutputProxy>],
    ) -> Result<(), JobExecError>;
}

impl<T: ?Sized + OperatorCore> OperatorCore for Box<T> {
    fn on_receive(
        &mut self, inputs: &[Box<dyn InputProxy>], outputs: &[Box<dyn OutputProxy>],
    ) -> Result<(), JobExecError> {
        (**self).on_receive(inputs, outputs)
    }
}

impl<T: OperatorCore> OperatorCore for DefaultNotifyOperator<T> {
    #[inline]
    fn on_receive(
        &mut self, inputs: &[Box<dyn InputProxy>], outputs: &[Box<dyn OutputProxy>],
    ) -> Result<(), JobExecError> {
        self.op.on_receive(inputs, outputs)
    }
}

pub trait NotifiableOperator: Notifiable + OperatorCore {}

impl<T: ?Sized + OperatorCore + Notifiable> NotifiableOperator for T {}

pub enum GeneralOperator {
    Simple(Box<dyn OperatorCore>),
    Notifiable(Box<dyn NotifiableOperator>),
}

/// 算子调度的输入条件：
///
/// 1. input 有数据 ;
/// 2. operator 是 active ;
/// 3. output 有capacity ;
///
/// 可调度判断：
/// 3 and (1 or 2)
///
///
pub struct Operator {
    pub info: OperatorInfo,
    inputs: Vec<Box<dyn InputProxy>>,
    outputs: Vec<Box<dyn OutputProxy>>,
    core: Box<dyn NotifiableOperator>,
    fire_times: u128,
    exec_st: Cell<u128>,
}

impl Operator {
    pub fn has_outstanding(&self) -> IOResult<bool> {
        for input in self.inputs.iter() {
            if input.has_outstanding()? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn is_finished(&self) -> bool {
        for output in self.outputs.iter() {
            if !output.get_blocks().is_empty() {
                return false;
            }
        }

        self.inputs.is_empty() || self.inputs.iter().all(|i| i.is_exhaust())
    }

    pub fn is_idle(&self) -> IOResult<bool> {
        for output in self.outputs.iter() {
            if !output.get_blocks().is_empty() {
                return Ok(false);
            }
        }

        Ok(!self.has_outstanding()?)
    }

    #[inline]
    pub fn fire(&mut self) -> Result<(), JobExecError> {
        let _f = Finally::new(&self.exec_st);
        debug_worker!("fire operator {:?}", self.info);
        self.fire_times += 1;

        for output in self.outputs.iter() {
            output.try_unblock()?;
        }

        let result = self
            .core
            .on_receive(&self.inputs, &self.outputs);

        for output in self.outputs.iter() {
            let blocks = output.get_blocks();
            for bs in blocks.iter() {
                for (index, input) in self.inputs.iter().enumerate() {
                    if !bs.has_block(index) {
                        let res = input.block(bs.tag());
                        bs.block(index, res);
                    }
                }
            }
        }

        let mut r = Ok(());
        if let Err(err) = result {
            if !err.can_be_retried() {
                return Err(err);
            } else {
                r = Err(err);
            }
        };

        for (port, input) in self.inputs.iter().enumerate() {
            while let Some(end) = input.extract_end() {
                let (tag, weight, _) = end.take();
                let notification = EndScope { port, tag, weight };
                self.core
                    .on_notify(notification, &self.outputs)?;
            }
        }

        for output in self.outputs.iter() {
            output.flush()?;
        }
        debug_worker!("after fire operator {:?}", self.info);
        r
    }

    pub fn cancel(&mut self, port: usize, tag: Tag) -> Result<(), JobExecError> {
        trace_worker!(
            "EARLY-STOP output[{:?}] stop sending data of scope {:?};",
            Port::new(self.info.index, port),
            tag,
        );
        self.outputs[port].cancel(&tag)?;
        let cancel = CancelScope { port, tag };
        self.core.on_cancel(cancel, &self.inputs)?;
        Ok(())
    }

    pub fn close(&self) {
        for output in self.outputs.iter() {
            if let Err(err) = output.close() {
                warn_worker!("close operator {:?}'s output failure, caused by {}", self.info, err);
            }
        }
        debug_worker!(
            "operator {:?}\tfinished, used {:.2}ms, fired {} times, avg fire use {}us",
            self.info,
            self.exec_st.get() as f64 / 1000.0,
            self.fire_times,
            self.exec_st.get() / self.fire_times
        );
    }
}

pub struct OperatorBuilder {
    pub info: OperatorInfo,
    inputs: Vec<Box<dyn InputProxy>>,
    inputs_notify: Vec<Option<Box<dyn InputEndNotify>>>,
    outputs: Vec<Box<dyn OutputBuilder>>,
    core: GeneralOperator,
}

impl OperatorBuilder {
    pub fn new(meta: OperatorInfo, core: GeneralOperator) -> Self {
        OperatorBuilder { info: meta, inputs: vec![], inputs_notify: vec![], outputs: vec![], core }
    }

    pub fn index(&self) -> usize {
        self.info.index
    }

    pub(crate) fn add_input<T: Data>(
        &mut self, ch_info: ChannelInfo, pull: GeneralPull<MicroBatch<T>>,
        notify: Option<GeneralPush<MicroBatch<T>>>, event_emitter: &EventEmitter,
    ) {
        assert_eq!(ch_info.target_port.port, self.inputs.len());
        let input = new_input(ch_info, pull, event_emitter);
        self.inputs.push(input);
        let n = notify.map(|p| Box::new(p) as Box<dyn InputEndNotify>);
        self.inputs_notify.push(n);
    }

    pub(crate) fn next_input_port(&self) -> Port {
        Port::new(self.info.index, self.inputs.len())
    }

    pub(crate) fn new_output_port<D: Data>(
        &mut self, batch_size: usize, scope_capacity: u32, batch_capacity: u32,
    ) -> OutputBuilderImpl<D> {
        let port = Port::new(self.info.index, self.outputs.len());
        let output =
            OutputBuilderImpl::new(port, self.info.scope_level, batch_size, batch_capacity, scope_capacity);
        self.outputs.push(Box::new(output.clone()));
        output
    }

    pub(crate) fn take_inputs_notify(&mut self) -> Vec<Option<Box<dyn InputEndNotify>>> {
        std::mem::replace(&mut self.inputs_notify, vec![])
    }

    pub(crate) fn build_outputs_cancel(&self) -> Vec<Option<OutputCancelState>> {
        let mut vec = Vec::with_capacity(self.outputs.len());
        for o in self.outputs.iter() {
            let handle = o.build_cancel_handle();
            vec.push(handle);
        }
        vec
    }

    pub(crate) fn build(self) -> Operator {
        let mut outputs = Vec::new();
        for ob in self.outputs {
            if let Some(o) = ob.build() {
                outputs.push(o);
            }
        }

        let core = match self.core {
            GeneralOperator::Simple(op) => {
                let scope_level = self.info.scope_level;
                let input_size = self.inputs.len();
                let output_size = outputs.len();
                let op = DefaultNotifyOperator::new(input_size, output_size, scope_level, op);
                Box::new(op) as Box<dyn NotifiableOperator>
            }
            GeneralOperator::Notifiable(op) => op,
        };
        Operator {
            info: self.info,
            inputs: self.inputs,
            outputs,
            core,
            fire_times: 0,
            exec_st: Cell::new(0),
        }
    }
}

struct Finally<'a> {
    exec_st: &'a Cell<u128>,
    start: Instant,
}

impl<'a> Finally<'a> {
    pub fn new(exec_st: &'a Cell<u128>) -> Self {
        Finally { exec_st, start: Instant::now() }
    }
}

impl<'a> Drop for Finally<'a> {
    fn drop(&mut self) {
        let s = self.exec_st.get() + self.start.elapsed().as_micros();
        self.exec_st.set(s);
    }
}

mod concise;
mod iteration;
mod primitives;
