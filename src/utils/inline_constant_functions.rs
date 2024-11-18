use hugr::{
    builder::{Dataflow, DataflowHugr, FunctionBuilder},
    extension::ExtensionRegistry,
    hugr::hugrmut::HugrMut,
    ops::{DataflowParent, FuncDefn, LoadFunction, Value},
    types::{PolyFuncType, Signature},
    HugrView, IncomingPort, Node, NodeIndex as _,
};

use anyhow::{anyhow, bail, Result};

fn const_fn_name(konst_n: Node) -> String {
    format!("const_fun_{}", konst_n.index())
}

pub fn inline_constant_functions(
    hugr: &mut impl HugrMut,
    registry: &ExtensionRegistry,
) -> Result<()> {
    while inline_constant_functions_impl(hugr, registry)? {}
    Ok(())
}

fn inline_constant_functions_impl(
    hugr: &mut impl HugrMut,
    registry: &ExtensionRegistry,
) -> Result<bool> {
    let mut const_funs = vec![];

    for n in hugr.nodes() {
        let (konst_hugr, sig) = {
            let Some(konst) = hugr.get_optype(n).as_const() else {
                continue;
            };
            let Value::Function { hugr } = konst.value() else {
                continue;
            };
            let optype = hugr.get_optype(hugr.root());
            if let Some(func) = optype.as_func_defn() {
                (hugr.as_ref().clone(), func.inner_signature())
            } else if let Some(dfg) = optype.as_dfg() {
                let signature: Signature = dfg.inner_signature();
                let mut builder = FunctionBuilder::new(const_fn_name(n), signature.clone())?;
                let outputs = builder
                    .add_hugr_view_with_wires(hugr, builder.input_wires())?
                    .outputs();
                (
                    builder.finish_hugr_with_outputs(outputs, registry)?,
                    signature,
                )
            } else {
                bail!(
                    "Constant function has unsupported root: {:?}",
                    hugr.get_optype(hugr.root())
                )
            }
        };
        let mut lcs = vec![];
        for load_constant in hugr.output_neighbours(n) {
            if !hugr.get_optype(load_constant).is_load_constant() {
                bail!(
                    "Constant function has non-LoadConstant output-neighbour: {load_constant} {:?}",
                    hugr.get_optype(load_constant)
                )
            }
            lcs.push(load_constant);
        }
        const_funs.push((n, konst_hugr.as_ref().clone(), sig, lcs));
    }

    let mut any_changes = false;

    for (konst_n, func_hugr, sig, load_constant_ns) in const_funs {
        if !load_constant_ns.is_empty() {
            let func_node = hugr.insert_hugr(hugr.root(), func_hugr).new_root;

            for lcn in load_constant_ns {
                hugr.disconnect(lcn, IncomingPort::from(0));
                hugr.replace_op(
                    lcn,
                    LoadFunction::try_new(sig.clone().into(), [], registry)?,
                )?;
                hugr.connect(func_node, 0, lcn, 0);
            }
            any_changes = true;
        }
        hugr.remove_node(konst_n);
    }
    Ok(any_changes)
}

#[cfg(test)]
mod test {
    use hugr::{
        builder::{
            Container, DFGBuilder, Dataflow, DataflowHugr, DataflowSubContainer, HugrBuilder,
            ModuleBuilder,
        },
        extension::{prelude::QB_T, PRELUDE_REGISTRY},
        ops::{CallIndirect, Const, Value},
        types::Signature,
        Hugr, HugrView, Wire,
    };

    use super::inline_constant_functions;

    fn build_const(go: impl FnOnce(&mut DFGBuilder<Hugr>) -> Wire) -> Const {
        Value::function({
            let mut builder = DFGBuilder::new(Signature::new_endo(QB_T)).unwrap();
            let r = go(&mut builder);
            builder
                .finish_hugr_with_outputs([r], &PRELUDE_REGISTRY)
                .unwrap()
        })
        .unwrap()
        .into()
    }

    #[test]
    fn simple() {
        let qb_sig: Signature = Signature::new_endo(QB_T);
        let mut hugr = {
            let mut builder = ModuleBuilder::new();
            let const_node = builder.add_constant(build_const(|builder| {
                let [r] = builder.input_wires_arr();
                r
            }));
            {
                let mut builder = builder.define_function("main", qb_sig.clone()).unwrap();
                let [i] = builder.input_wires_arr();
                let fun = builder.load_const(&const_node);
                let [r] = builder
                    .add_dataflow_op(
                        CallIndirect {
                            signature: qb_sig.clone(),
                        },
                        [fun, i],
                    )
                    .unwrap()
                    .outputs_arr();
                builder.finish_with_outputs([r]).unwrap();
            };
            builder.finish_hugr(&PRELUDE_REGISTRY).unwrap()
        };

        inline_constant_functions(&mut hugr, &PRELUDE_REGISTRY).unwrap();

        for n in hugr.nodes() {
            if let Some(konst) = hugr.get_optype(n).as_const() {
                assert!(!matches!(konst.value(), Value::Function { .. }))
            }
        }
    }

    #[test]
    fn nested() {
        let qb_sig: Signature = Signature::new_endo(QB_T);
        let mut hugr = {
            let mut builder = ModuleBuilder::new();
            let const_node = builder.add_constant(build_const(|builder| {
                let [i] = builder.input_wires_arr();
                let func = builder.add_load_const(build_const(|builder| {
                    let [r] = builder.input_wires_arr();
                    r
                }));
                let [r] = builder
                    .add_dataflow_op(
                        CallIndirect {
                            signature: qb_sig.clone(),
                        },
                        [func, i],
                    )
                    .unwrap()
                    .outputs_arr();
                r
            }));
            {
                let mut builder = builder.define_function("main", qb_sig.clone()).unwrap();
                let [i] = builder.input_wires_arr();
                let fun = builder.load_const(&const_node);
                let [r] = builder
                    .add_dataflow_op(
                        CallIndirect {
                            signature: qb_sig.clone(),
                        },
                        [fun, i],
                    )
                    .unwrap()
                    .outputs_arr();
                builder.finish_with_outputs([r]).unwrap();
            };
            builder.finish_hugr(&PRELUDE_REGISTRY).unwrap()
        };

        inline_constant_functions(&mut hugr, &PRELUDE_REGISTRY).unwrap();

        for n in hugr.nodes() {
            if let Some(konst) = hugr.get_optype(n).as_const() {
                assert!(!matches!(konst.value(), Value::Function { .. }))
            }
        }
    }
}
