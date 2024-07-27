use std::{collections::HashMap, rc::Rc};

use anyhow::{anyhow, Result};
use hugr::{
    ops::{FuncDecl, FuncDefn, OpType},
    types::Type,
    HugrView, NodeIndex, PortIndex, Wire,
};
use inkwell::{
    basic_block::BasicBlock, builder::Builder, context::Context, debug_info::{AsDIScope, DIScope, DebugInfoBuilder}, types::{BasicType, BasicTypeEnum, FunctionType}, values::{FunctionValue, GlobalValue}
};
use itertools::{zip_eq, Itertools as _};

use crate::{debuginfo::{func_debug_info, op_debug_location}, types::{HugrFuncType, HugrSumType, HugrType, TypingSession}};
use crate::{custom::CodegenExtsMap, fat::FatNode, types::LLVMSumType};
use delegate::delegate;

use self::mailbox::ValueMailBox;

use super::{EmissionSet, EmitModuleContext, EmitOpArgs};

mod mailbox;
pub use mailbox::{RowMailBox, RowPromise};

/// A context for emitting an LLVM function.
///
/// One of the primary interfaces that impls of
/// [crate::custom::CodegenExtension] and [super::EmitOp] will interface with,
/// we have methods for:
///   * Converting from hugr [Type]s to LLVM [Type](BasicTypeEnum)s;
///   * Maintaing [MailBox](RowMailBox) for each [Wire] in the [FuncDefn];
///   * Accessing the [CodegenExtsMap];
///   * Accessing an in internal [Builder].
///
/// The internal [Builder] must always be positioned at the end of a
/// [BasicBlock]. This invariant is not checked when the builder is accessed
/// through [EmitFuncContext::builder].
///
/// [MailBox](RowMailBox)es are stack allocations that are `alloca`ed in the
/// first basic block of the function, read from to get the input values of each
/// node, and written to with the output values of each node.
pub struct EmitFuncContext<'c, H> {
    emit_context: EmitModuleContext<'c, H>,
    todo: EmissionSet<'c, H>,
    func: FunctionValue<'c>,
    env: HashMap<Wire, ValueMailBox<'c>>,
    builder: Builder<'c>,
    prologue_bb: BasicBlock<'c>,
    launch_bb: BasicBlock<'c>,
    scope: DIScope<'c>,
    di_builder: Rc<DebugInfoBuilder<'c>>,

}

impl<'c, H: HugrView> EmitFuncContext<'c, H> {
    delegate! {
        to self.emit_context {
            /// Returns the inkwell [Context].
            pub fn iw_context(&self) ->  &'c Context;
            /// Returns the internal [CodegenExtsMap] .
            pub fn extensions(&self) ->  Rc<CodegenExtsMap<'c,H>>;
            /// Returns a new [TypingSession].
            pub fn typing_session(&self) -> TypingSession<'c,H>;
            /// Convert hugr [HugrType] into an LLVM [Type](BasicTypeEnum).
            pub fn llvm_type(&self, hugr_type: &HugrType) -> Result<BasicTypeEnum<'c> >;
            /// Convert a [HugrFuncType] into an LLVM [FunctionType].
            pub fn llvm_func_type(&self, hugr_type: &HugrFuncType) -> Result<FunctionType<'c> >;
            /// Convert a hugr [HugrSumType] into an LLVM [LLVMSumType].
            pub fn llvm_sum_type(&self, sum_ty: HugrSumType) -> Result<LLVMSumType<'c>>;
            /// Adds or gets the [FunctionValue] in the [inkwell::module::Module] corresponding to the given [FuncDefn].
            ///
            /// The name of the result may have been mangled.
            pub fn get_func_defn(&self, node: FatNode<'c, FuncDefn, H>) -> Result<FunctionValue<'c>>;
            /// Adds or gets the [FunctionValue] in the [inkwell::module::Module] corresponding to the given [FuncDecl].
            ///
            /// The name of the result may have been mangled.
            pub fn get_func_decl(&self, node: FatNode<'c, FuncDecl, H>) -> Result<FunctionValue<'c>>;
            /// Adds or get the [FunctionValue] in the [inkwell::module::Module] with the given symbol
            /// and function type.
            ///
            /// The name undergoes no mangling. The [FunctionValue] will have
            /// [inkwell::module::Linkage::External].
            ///
            /// If this function is called multiple times with the same arguments it
            /// will return the same [FunctionValue].
            ///
            /// If a function with the given name exists but the type does not match
            /// then an Error is returned.
            pub fn get_extern_func(&self, symbol: impl AsRef<str>, typ: FunctionType<'c>,) -> Result<FunctionValue<'c>>;
            /// Adds or gets the [GlobalValue] in the [inkwell::module::Module] corresponding to the
            /// given symbol and LLVM type.
            ///
            /// The name will not be mangled.
            ///
            /// If a global with the given name exists but the type or constant-ness
            /// does not match then an error will be returned.
            pub fn get_global(&self, symbol: impl AsRef<str>, typ: impl BasicType<'c>, constant: bool) -> Result<GlobalValue<'c>>;
        }
    }

    pub fn clear_debug_location(&self) {
        self.builder().unset_current_debug_location();
    }

    pub fn set_debug_location(&self, node: FatNode<'c, OpType, H>, scope: Option<DIScope<'c>>) {
        let location = op_debug_location(self.iw_context(), &self.di_builder, scope.unwrap_or(self.scope), node);
        self.builder().set_current_debug_location(location);
    }

    /// Used when emitters encounter a scoped definition. `node` will be
    /// returned from [EmitFuncContext::finish].
    pub fn push_todo_func(&mut self, node: FatNode<'c, FuncDefn, H>) {
        self.todo.insert(node, self.scope);
    }

    // TODO likely we don't need this
    // pub fn func(&self) -> &FunctionValue<'c> {
    //     &self.func
    // }

    /// Returns the internal [Builder]. Callers must ensure that it is
    /// positioned at the end of a basic block. This invariant is not checked(it
    /// doesn't seem possible to check it).
    pub fn builder(&self) -> &Builder<'c> {
        &self.builder
    }

    /// Create a new basic block. When `before` is `Some` the block will be
    /// created immediately before that block, otherwise at the end of the
    /// function.
    pub(crate) fn new_basic_block(
        &mut self,
        name: impl AsRef<str>,
        before: Option<BasicBlock<'c>>,
    ) -> BasicBlock<'c> {
        if let Some(before) = before {
            self.iw_context().prepend_basic_block(before, name.as_ref())
        } else {
            self.iw_context()
                .append_basic_block(self.func, name.as_ref())
        }
    }

    fn prologue_block(&self) -> BasicBlock<'c> {
        // guaranteed to exist because we create it in new
        self.func.get_first_basic_block().unwrap()
    }

    /// Creates a new `EmitFuncContext` for `func`, taking ownership of
    /// `emit_context`. `emit_context` will be returned in
    /// [EmitFuncContext::finish].
    ///
    /// If `func` has any existing [BasicBlock]s we will fail.
    ///
    /// TODO on failure return `emit_context`
    pub fn new(
        emit_context: EmitModuleContext<'c, H>,
        func: FunctionValue<'c>,
        di_builder: Rc<DebugInfoBuilder<'c>>,
        scope: DIScope<'c>,
    ) -> Result<EmitFuncContext<'c, H>> {
        if func.get_first_basic_block().is_some() {
            Err(anyhow!(
                "EmitContext::new: Function already has a basic block: {:?}",
                func.get_name()
            ))?;
        }

        let prologue_bb = emit_context
            .iw_context()
            .append_basic_block(func, "alloca_block");
        let launch_bb = emit_context
            .iw_context()
            .append_basic_block(func, "entry_block");
        let builder = emit_context.iw_context().create_builder();
        builder.position_at_end(launch_bb);
        Ok(Self {
            emit_context,
            todo: Default::default(),
            func,
            env: Default::default(),
            builder,
            prologue_bb,
            launch_bb,
            scope,
            di_builder
        })
    }

    fn new_value_mail_box(&mut self, t: &Type, name: impl AsRef<str>) -> Result<ValueMailBox<'c>> {
        let bte = self.llvm_type(t)?;
        let ptr = self.build_prologue(|builder| builder.build_alloca(bte, name.as_ref()))?;
        Ok(ValueMailBox::new(bte, ptr, Some(name.as_ref().into())))
    }

    /// Create a new anonymous [RowMailBox]. This mailbox is not mapped to any
    /// [Wire]s, and so will not interact with any mailboxes returned from
    /// [EmitFuncContext::node_ins_rmb] or [EmitFuncContext::node_outs_rmb].
    pub fn new_row_mail_box<'a>(
        &mut self,
        ts: impl IntoIterator<Item = &'a Type>,
        name: impl AsRef<str>,
    ) -> Result<RowMailBox<'c>> {
        Ok(RowMailBox::new(
            ts.into_iter()
                .enumerate()
                .map(|(i, t)| self.new_value_mail_box(t, format!("{i}")))
                .collect::<Result<Vec<_>>>()?,
            Some(name.as_ref().into()),
        ))
    }

    fn build_prologue<T>(&mut self, f: impl FnOnce(&Builder<'c>) -> T) -> T {
        let b = self.prologue_block();
        self.build_positioned(b, |x| f(&x.builder))
    }

    /// Creates a new [BasicBlock] and calls `f` with the internal builder
    /// positioned at the start of that [BasicBlock]. The builder will be
    /// repositioned back to it's location before `f` before this function
    /// returns.
    pub fn build_positioned_new_block<T>(
        &mut self,
        name: impl AsRef<str>,
        before: Option<BasicBlock<'c>>,
        f: impl FnOnce(&mut Self, BasicBlock<'c>) -> T,
    ) -> T {
        let bb = self.new_basic_block(name, before);
        self.build_positioned(bb, |s| f(s, bb))
    }

    /// Positions the internal builder at the end of `block` and calls `f`.  The
    /// builder will be repositioned back to it's location before `f` before
    /// this function returns.
    pub fn build_positioned<T>(
        &mut self,
        block: BasicBlock<'c>,
        f: impl FnOnce(&mut Self) -> T,
    ) -> T {
        // safe because our builder is always positioned
        let current = self.builder.get_insert_block().unwrap();
        self.builder.position_at_end(block);
        let r = f(self);
        self.builder.position_at_end(current);
        r
    }

    /// Returns a [RowMailBox] mapped to thie input wires of `node`. When emitting a node
    /// input values are from this mailbox.
    pub fn node_ins_rmb<OT: 'c>(&mut self, node: FatNode<'c, OT, H>) -> Result<RowMailBox<'c>> {
        let r = node
            .in_value_types()
            .map(|(p, t)| {
                let (slo_n, slo_p) = node
                    .single_linked_output(p)
                    .ok_or(anyhow!("No single linked output"))?;
                self.map_wire(slo_n, slo_p, &t)
            })
            .collect::<Result<RowMailBox>>()?;

        debug_assert!(zip_eq(node.in_value_types(), r.get_types())
            .all(|((_, t), lt)| self.llvm_type(&t).unwrap() == lt));
        Ok(r)
    }

    /// Returns a [RowMailBox] mapped to thie ouput wires of `node`. When emitting a node
    /// output values are written to this mailbox.
    pub fn node_outs_rmb<OT: 'c>(&mut self, node: FatNode<'c, OT, H>) -> Result<RowMailBox<'c>> {
        let r = node
            .out_value_types()
            .map(|(port, hugr_type)| self.map_wire(node, port, &hugr_type))
            .collect::<Result<RowMailBox>>()?;
        #[cfg(debug_assertions)]
        {
            let ts1 = node.out_value_types().map(|x| self.llvm_type(&x.1).unwrap()).collect_vec();
            let ts2 = r.get_types().collect_vec();
            debug_assert_eq!(ts1,ts2);
        }
        debug_assert!(zip_eq(node.out_value_types(), r.get_types())
            .all(|((_, t), lt)| self.llvm_type(&t).unwrap() == lt));
        Ok(r)
    }

    fn map_wire<OT>(
        &mut self,
        node: FatNode<'c, OT, H>,
        port: hugr::OutgoingPort,
        hugr_type: &Type,
    ) -> Result<ValueMailBox<'c>> {
        let wire = Wire::new(node.node(), port);
        if let Some(mb) = self.env.get(&wire) {
            debug_assert_eq!(self.llvm_type(hugr_type).unwrap(), mb.get_type());
            return Ok(mb.clone());
        }
        let mb = self.new_value_mail_box(
            hugr_type,
            format!("{}_{}", node.node().index(), port.index()),
        )?;
        self.env.insert(wire, mb.clone());
        Ok(mb)
    }

    /// Consumes the `EmitFuncContext` and returns both the inner
    /// [EmitModuleContext] and the scoped [FuncDefn]s that were encountered.
    pub fn finish(self) -> Result<(EmitModuleContext<'c, H>, EmissionSet<'c, H>)> {
        self.builder.position_at_end(self.prologue_bb);
        self.builder.build_unconditional_branch(self.launch_bb)?;
        self.di_builder.finalize();
        Ok((self.emit_context, self.todo))
    }
}

pub fn emit_func<'c,H: HugrView>(
    emit_context: EmitModuleContext<'c, H>,
    node: FatNode<'c, FuncDefn, H>,
    di_builder: Rc<DebugInfoBuilder<'c>>,
    scope: DIScope<'c>,
) -> Result<(EmitModuleContext<'c, H>, EmissionSet<'c, H>)> {
    let func = emit_context.get_func_defn(node)?;
    let di_subprogram = func_debug_info(&di_builder, &emit_context.namer, scope, node);
    func.set_subprogram(di_subprogram);

    let mut func_ctx = EmitFuncContext::new(emit_context, func, di_builder, di_subprogram.as_debug_info_scope())?;
    let ret_rmb = func_ctx.new_row_mail_box(node.signature.body().output.iter(), "ret")?;
    super::ops::emit_dataflow_parent(
        &mut func_ctx,
        EmitOpArgs {
            node,
            inputs: func.get_params(),
            outputs: ret_rmb.promise(),
        },
    )?;
    let builder = func_ctx.builder();
    match &ret_rmb.read::<Vec<_>>(builder, [])?[..] {
        [] => builder.build_return(None)?,
        [x] => builder.build_return(Some(x))?,
        xs => builder.build_aggregate_return(xs)?,
    };
    func_ctx.finish()
}
