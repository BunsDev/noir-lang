use crate::ssa::{
    context::SsaContext,
    mem::{MemArray, Memory},
    node::{BinaryOp, Instruction, Node, NodeId, NodeObject, ObjectType, Operation},
    {builtin, mem, node},
};
use crate::{Evaluator, RuntimeErrorKind};
use acvm::{
    acir::circuit::opcodes::{BlackBoxFuncCall, FunctionInput, Opcode as AcirOpcode},
    acir::native_types::{Expression, Witness},
    FieldElement,
};
use iter_extended::vecmap;
use num_bigint::BigUint;
use num_traits::{One, Zero};
use std::collections::HashMap;

mod internal_var;
pub(crate) use internal_var::InternalVar;
mod constraints;
use constraints::to_radix_base;
// Expose this to the crate as we need to apply range constraints when
// converting the ABI(main parameters) to Noir types
pub(crate) use constraints::range_constraint;
mod intrinsics;
mod memory_map;
use memory_map::MemoryMap;

#[derive(Default)]
pub struct Acir {
    memory_map: MemoryMap,
    arith_cache: HashMap<NodeId, InternalVar>,
}

impl Acir {
    //This function stores the substitution with the arithmetic expression in the cache
    //When an instruction performs arithmetic operation, its output can be represented as an arithmetic expression of its arguments
    //Substitute a node object as an arithmetic expression
    // Returns `None` if `NodeId` represents an array pointer.
    fn node_id_to_internal_var(
        &mut self,
        id: NodeId,
        evaluator: &mut Evaluator,
        ctx: &SsaContext,
    ) -> Option<InternalVar> {
        if let Some(internal_var) = self.arith_cache.get(&id) {
            return Some(internal_var.clone());
        }

        let mut var = match ctx.try_get_node(id) {
            Some(NodeObject::Const(c)) => {
                let field_value = FieldElement::from_be_bytes_reduce(&c.value.to_bytes_be());
                InternalVar::from_constant(field_value)
            }
            Some(NodeObject::Obj(variable)) => {
                let variable_type = variable.get_type();
                match variable_type {
                    ObjectType::Pointer(_) => return None,
                    _ => {
                        let witness =
                            variable.witness.unwrap_or_else(|| evaluator.add_witness_to_cs());
                        InternalVar::from_witness(witness)
                    }
                }
            }
            _ => {
                let witness = evaluator.add_witness_to_cs();
                InternalVar::from_witness(witness)
            }
        };

        var.set_id(id);
        self.arith_cache.insert(id, var.clone());
        Some(var)
    }

    fn node_id_to_internal_var_unwrap(
        &mut self,
        id: NodeId,
        evaluator: &mut Evaluator,
        ctx: &SsaContext,
    ) -> InternalVar {
        self.node_id_to_internal_var(id, evaluator, ctx)
            .expect("ICE: `NodeId` type cannot be converted into an `InternalVar`")
    }

    fn get_predicate(
        &mut self,
        binary: &node::Binary,
        evaluator: &mut Evaluator,
        ctx: &SsaContext,
    ) -> InternalVar {
        let predicate_node_id = match binary.predicate {
            Some(pred) => pred,
            None => return InternalVar::from(Expression::one()),
        };

        self.node_id_to_internal_var_unwrap(predicate_node_id, evaluator, ctx)
    }
    pub fn evaluate_instruction(
        &mut self,
        ins: &Instruction,
        evaluator: &mut Evaluator,
        ctx: &SsaContext,
    ) -> Result<(), RuntimeErrorKind> {
        if ins.operation == Operation::Nop {
            return Ok(());
        }

        let output = match &ins.operation {
            Operation::Binary(binary) => {
                Some(self.evaluate_binary(binary, ins.res_type, evaluator, ctx))
            }
            Operation::Constrain(value, ..) => {
                let value = self.node_id_to_internal_var_unwrap(*value, evaluator, ctx);
                let subtract = constraints::subtract(
                    &Expression::one(),
                    FieldElement::one(),
                    value.expression(),
                );
                evaluator.opcodes.push(AcirOpcode::Arithmetic(subtract));
                Some(value)
            }
            Operation::Not(value) => {
                let a = (1_u128 << ins.res_type.bits()) - 1;
                let l_c = self.node_id_to_internal_var_unwrap(*value, evaluator, ctx);
                Some(
                    constraints::subtract(
                        &Expression::from(&FieldElement::from(a)),
                        FieldElement::one(),
                        l_c.expression(),
                    )
                    .into(),
                )
            }
            Operation::Cast(value) => self.node_id_to_internal_var(*value, evaluator, ctx),
            Operation::Truncate { value, bit_size, max_bit_size } => {
                let value = self.node_id_to_internal_var_unwrap(*value, evaluator, ctx);
                Some(InternalVar::from_expression(constraints::evaluate_truncate(
                    value.expression(),
                    *bit_size,
                    *max_bit_size,
                    evaluator,
                )))
            }
            Operation::Intrinsic(opcode, args) => {
                let v = self.evaluate_opcode(ins.id, *opcode, args, ins.res_type, ctx, evaluator);
                Some(InternalVar::from(v))
            }
            Operation::Return(node_ids) => {
                // XXX: When we return a node_id that was created from
                // the UnitType, there is a witness associated with it
                // Ideally no witnesses are created for such types.

                // This can only ever be called in the main context.
                // In all other context's, the return operation is transformed.

                for node_id in node_ids {
                    // An array produces a single node_id
                    // We therefore need to check if the node_id is referring to an array
                    // and deference to get the elements
                    let objects = match Memory::deref(ctx, *node_id) {
                        Some(a) => {
                            let array = &ctx.mem[a];
                            self.memory_map.load_array(array)
                        }
                        None => vec![self.node_id_to_internal_var_unwrap(*node_id, evaluator, ctx)],
                    };

                    for mut object in objects {
                        let witness = object
                            .get_or_compute_witness(evaluator, true)
                            .expect("infallible: `None` can only be returned when we disallow constant Expressions.");
                        // Before pushing to the public inputs, we need to check that
                        // it was not a private ABI input
                        if evaluator.is_private_abi_input(witness) {
                            return Err(RuntimeErrorKind::Spanless(String::from(
                                "we do not allow private ABI inputs to be returned as public outputs",
                            )));
                        }
                        evaluator.public_inputs.push(witness);
                    }
                }

                None
            }
            Operation::Cond { condition, val_true: lhs, val_false: rhs } => {
                let cond = self.node_id_to_internal_var_unwrap(*condition, evaluator, ctx);
                let l_c = self.node_id_to_internal_var_unwrap(*lhs, evaluator, ctx);
                let r_c = self.node_id_to_internal_var_unwrap(*rhs, evaluator, ctx);
                let sub =
                    constraints::subtract(l_c.expression(), FieldElement::one(), r_c.expression());
                let result = constraints::add(
                    &constraints::mul_with_witness(evaluator, cond.expression(), &sub),
                    FieldElement::one(),
                    r_c.expression(),
                );
                Some(result.into())
            }
            Operation::Nop => None,
            Operation::Load { array_id, index } => {
                //retrieves the value from the map if address is known at compile time:
                //address = l_c and should be constant
                let index = self.node_id_to_internal_var_unwrap(*index, evaluator, ctx);

                let array_element = match index.to_const() {
                    Some(index) => {
                        let idx = mem::Memory::as_u32(index);
                        let mem_array = &ctx.mem[*array_id];

                        self.memory_map.load_array_element_constant_index(mem_array, idx).expect(
                            "ICE: index {idx} was out of bounds for array of length {mem_array.len}",
                        )
                    }
                    None => unimplemented!("dynamic arrays are not implemented yet"),
                };
                Some(array_element)
            }
            Operation::Store { array_id, index, value } => {
                //maps the address to the rhs if address is known at compile time
                let index = self.node_id_to_internal_var_unwrap(*index, evaluator, ctx);
                let value = self.node_id_to_internal_var_unwrap(*value, evaluator, ctx);

                match index.to_const() {
                    Some(index) => {
                        let idx = mem::Memory::as_u32(index);
                        let absolute_adr = ctx.mem[*array_id].absolute_adr(idx);
                        self.memory_map.insert(absolute_adr, value);
                        //we do not generate constraint, so no output.
                        None
                    }
                    None => todo!("dynamic arrays are not implemented yet"),
                }
            }
            i @ Operation::Jne(..)
            | i @ Operation::Jeq(..)
            | i @ Operation::Jmp(_)
            | i @ Operation::Phi { .. }
            | i @ Operation::Call { .. }
            | i @ Operation::Result { .. } => {
                unreachable!("Invalid instruction: {:?}", i);
            }
        };

        // If the output returned an `InternalVar` then we add it to the cache
        if let Some(mut output) = output {
            output.set_id(ins.id);
            self.arith_cache.insert(ins.id, output);
        }

        Ok(())
    }

    fn evaluate_binary(
        &mut self,
        binary: &node::Binary,
        res_type: ObjectType,
        evaluator: &mut Evaluator,
        ctx: &SsaContext,
    ) -> InternalVar {
        let r_size = ctx[binary.rhs].size_in_bits();
        let l_size = ctx[binary.lhs].size_in_bits();
        let max_size = u32::max(r_size, l_size);

        match &binary.operator {
            BinaryOp::Add | BinaryOp::SafeAdd => {
                let l_c = self.node_id_to_internal_var_unwrap(binary.lhs, evaluator, ctx);
                let r_c = self.node_id_to_internal_var_unwrap(binary.rhs, evaluator, ctx);
                
                InternalVar::from(constraints::add(
                    l_c.expression(),
                    FieldElement::one(),
                    r_c.expression(),
                ))
                
            },
            BinaryOp::Sub { max_rhs_value } | BinaryOp::SafeSub { max_rhs_value } => {
                                let l_c = self.node_id_to_internal_var_unwrap(binary.lhs, evaluator, ctx);
                let r_c = self.node_id_to_internal_var_unwrap(binary.rhs, evaluator, ctx);
                
               
                if res_type == ObjectType::NativeField {
                    InternalVar::from(constraints::subtract(
                        l_c.expression(),
                        FieldElement::one(),
                        r_c.expression(),
                    ))
                } else {
                    //we need the type of rhs and its max value, then:
                    //lhs-rhs+k*2^bit_size where k=ceil(max_value/2^bit_size)
                    // TODO: what is this code doing?
                    let bit_size = r_size;
                    let r_big = BigUint::one() << bit_size;
                    let mut k = max_rhs_value / &r_big;
                    if max_rhs_value % &r_big != BigUint::zero() {
                        k = &k + BigUint::one();
                    }
                    k = &k * r_big;
                    let f = FieldElement::from_be_bytes_reduce(&k.to_bytes_be());
                    let mut sub_expr = constraints::subtract(
                        l_c.expression(),
                        FieldElement::one(),
                        r_c.expression(),
                    );
                    sub_expr.q_c += f;
                    let mut sub_var = sub_expr.into();
                    //TODO: uses interval analysis for more precise check
                    if let Some(lhs_const) = l_c.to_const() {
                        if max_rhs_value <= &BigUint::from_bytes_be(&lhs_const.to_be_bytes()) {
                            sub_var = InternalVar::from(constraints::subtract(
                                l_c.expression(),
                                FieldElement::one(),
                                r_c.expression(),
                            ));
                        }
                    }
                    sub_var
                }
            }
            BinaryOp::Mul | BinaryOp::SafeMul => {
                                let l_c = self.node_id_to_internal_var_unwrap(binary.lhs, evaluator, ctx);
                let r_c = self.node_id_to_internal_var_unwrap(binary.rhs, evaluator, ctx);
                
                
                InternalVar::from(constraints::mul_with_witness(
                evaluator,
                l_c.expression(),
                r_c.expression(),
            ))
            },
            BinaryOp::Udiv => {
                                let l_c = self.node_id_to_internal_var_unwrap(binary.lhs, evaluator, ctx);
                let r_c = self.node_id_to_internal_var_unwrap(binary.rhs, evaluator, ctx);
                
                let predicate = self.get_predicate(binary, evaluator, ctx);
                
                let (q_wit, _) = constraints::evaluate_udiv(
                    l_c.expression(),
                    r_c.expression(),
                    max_size,
                    predicate.expression(),
                    evaluator,
                );
                InternalVar::from(q_wit)
            }
            BinaryOp::Sdiv => {
                                let l_c = self.node_id_to_internal_var_unwrap(binary.lhs, evaluator, ctx);
                let r_c = self.node_id_to_internal_var_unwrap(binary.rhs, evaluator, ctx);
                
                InternalVar::from(
                constraints::evaluate_sdiv(l_c.expression(), r_c.expression(), evaluator).0,
            )
        },
            BinaryOp::Urem => {
                                let l_c = self.node_id_to_internal_var_unwrap(binary.lhs, evaluator, ctx);
                let r_c = self.node_id_to_internal_var_unwrap(binary.rhs, evaluator, ctx);
                
                let predicate = self.get_predicate(binary, evaluator, ctx);
                let (_, r_wit) = constraints::evaluate_udiv(
                    l_c.expression(),
                    r_c.expression(),
                    max_size,
                    predicate.expression(),
                    evaluator,
                );
                InternalVar::from(r_wit)
            }
            BinaryOp::Srem => {
                
                                let l_c = self.node_id_to_internal_var_unwrap(binary.lhs, evaluator, ctx);
                let r_c = self.node_id_to_internal_var_unwrap(binary.rhs, evaluator, ctx);
                

                InternalVar::from(
                // TODO: we should use variable naming here instead of .1
                constraints::evaluate_sdiv(l_c.expression(), r_c.expression(), evaluator).1,
            )},
            BinaryOp::Div => {
                                let l_c = self.node_id_to_internal_var_unwrap(binary.lhs, evaluator, ctx);
                let mut r_c = self.node_id_to_internal_var_unwrap(binary.rhs, evaluator, ctx);
                
                let predicate = self.get_predicate(binary, evaluator, ctx).expression().clone();
                //TODO avoid creating witnesses here.
                let x_witness = r_c.get_or_compute_witness(evaluator, false).expect("unexpected constant expression"); 

                let inverse = expression_from_witness(constraints::evaluate_inverse(
                    x_witness, &predicate, evaluator,
                ));
                InternalVar::from(constraints::mul_with_witness(
                    evaluator,
                    l_c.expression(),
                    &inverse,
                ))
            }
            BinaryOp::Eq => {
                                let l_c = self.node_id_to_internal_var(binary.lhs, evaluator, ctx);
                let r_c = self.node_id_to_internal_var(binary.rhs, evaluator, ctx);
                
                InternalVar::from(
                self.evaluate_eq(binary.lhs, binary.rhs, l_c, r_c, ctx, evaluator),
            )},
            BinaryOp::Ne => {
                                let l_c = self.node_id_to_internal_var(binary.lhs, evaluator, ctx);
                let r_c = self.node_id_to_internal_var(binary.rhs, evaluator, ctx);
                
                
                InternalVar::from(
                self.evaluate_neq(binary.lhs, binary.rhs, l_c, r_c, ctx, evaluator),
            )},
            BinaryOp::Ult => {
                
                                let l_c = self.node_id_to_internal_var_unwrap(binary.lhs, evaluator, ctx);
                let r_c = self.node_id_to_internal_var_unwrap(binary.rhs, evaluator, ctx);
                
                let size = ctx[binary.lhs].get_type().bits();
                constraints::evaluate_cmp(
                    l_c.expression(),
                    r_c.expression(),
                    size,
                    false,
                    evaluator,
                )
                .into()
            }
            BinaryOp::Ule => {
                                let l_c = self.node_id_to_internal_var_unwrap(binary.lhs, evaluator, ctx);
                let r_c = self.node_id_to_internal_var_unwrap(binary.rhs, evaluator, ctx);
                
                let size = ctx[binary.lhs].get_type().bits();
                let e = constraints::evaluate_cmp(
                    r_c.expression(),
                    l_c.expression(),
                    size,
                    false,
                    evaluator,
                );
                constraints::subtract(&Expression::one(), FieldElement::one(), &e).into()
            }
            BinaryOp::Slt => {
                                let l_c = self.node_id_to_internal_var_unwrap(binary.lhs, evaluator, ctx);
                let r_c = self.node_id_to_internal_var_unwrap(binary.rhs, evaluator, ctx);
                
                let s = ctx[binary.lhs].get_type().bits();
                constraints::evaluate_cmp(l_c.expression(), r_c.expression(), s, true, evaluator)
                    .into()
            }
            BinaryOp::Sle => {
                                let l_c = self.node_id_to_internal_var_unwrap(binary.lhs, evaluator, ctx);
                let r_c = self.node_id_to_internal_var_unwrap(binary.rhs, evaluator, ctx);
                
                let s = ctx[binary.lhs].get_type().bits();
                let e = constraints::evaluate_cmp(
                    r_c.expression(),
                    l_c.expression(),
                    s,
                    true,
                    evaluator,
                );
                constraints::subtract(&Expression::one(), FieldElement::one(), &e).into()
            }
            BinaryOp::Lt | BinaryOp::Lte => {
                // TODO Create an issue to change this function to return a RuntimeErrorKind
                // TODO then replace `unimplemented` with an error
                // TODO (This is a breaking change)
                unimplemented!(
                "Field comparison is not implemented yet, try to cast arguments to integer type"
            )
            }
            BinaryOp::And | BinaryOp::Or | BinaryOp::Xor => {
                                let l_c = self.node_id_to_internal_var_unwrap(binary.lhs, evaluator, ctx);
                let r_c = self.node_id_to_internal_var_unwrap(binary.rhs, evaluator, ctx);
                
                let bit_size = res_type.bits();
                let opcode = binary.operator.clone();
                let bitwise_result = match simplify_bitwise(&l_c, &r_c, bit_size, &opcode) {
                    Some(simplified_internal_var) => simplified_internal_var.expression().clone(),
                    None => evaluate_bitwise(l_c, r_c, bit_size, evaluator, opcode),
                };
                InternalVar::from(bitwise_result)
            }
            BinaryOp::Shl | BinaryOp::Shr => unreachable!("ICE: ShiftLeft and ShiftRight are replaced by multiplications and divisions in optimization pass."),
            i @ BinaryOp::Assign => unreachable!("Invalid Instruction: {:?}", i),
        }
    }
    // Given two `NodeId`s, generate constraints to check whether
    // they are Equal.
    //
    // This method returns an `Expression` representing `0` or `1`
    // If the two `NodeId`s are not equal, then the `Expression`
    // returned will represent `1`, otherwise `0` is returned.
    //
    // A `NodeId` can represent a primitive data type
    // like a `Field` or it could represent a composite type like an
    // `Array`. Depending on the type, the constraints that will be generated
    // will differ.
    //
    // TODO(check this): Types like structs are decomposed before getting to SSA
    // so in reality, the NEQ instruction will be done on the fields
    // of the struct
    fn evaluate_neq(
        &mut self,
        lhs: NodeId,
        rhs: NodeId,
        l_c: Option<InternalVar>,
        r_c: Option<InternalVar>,
        ctx: &SsaContext,
        evaluator: &mut Evaluator,
    ) -> Expression {
        // Check whether the `lhs` and `rhs` are Arrays
        if let (Some(a), Some(b)) = (Memory::deref(ctx, lhs), Memory::deref(ctx, rhs)) {
            let array_a = &ctx.mem[a];
            let array_b = &ctx.mem[b];

            assert!(l_c.is_none());
            assert!(r_c.is_none());

            // TODO What happens if we call `l_c.expression()` on InternalVar
            // TODO when we know that they should correspond to Arrays
            // TODO(Guillaume): We can add an Option<Expression>  because
            // TODO when the object is composite, it will return One
            if array_a.len != array_b.len {
                unreachable!(
                    "ICE: arrays have differing lengths {} and {}. 
                We cannot compare two different types in Noir, 
                so this should have been caught by the type checker",
                    array_a.len, array_b.len
                )
            }

            let mut x = InternalVar::from(self.array_eq(array_a, array_b, evaluator));
            // TODO we need a witness because of the directive, but we should use an expression
            // TODO if we change the Invert directive to take an `Expression`, then we
            // TODO can get rid of this extra gate.
            let x_witness =
                x.get_or_compute_witness(evaluator, false).expect("unexpected constant expression");

            return expression_from_witness(constraints::evaluate_zero_equality(
                x_witness, evaluator,
            ));
        }
        let l_c = l_c.expect("ICE: unexpected array pointer");
        let r_c = r_c.expect("ICE: unexpected array pointer");
        // Arriving here means that `lhs` and `rhs` are not Arrays
        //
        // Check if `lhs` and `rhs` are constants. If so, we can evaluate whether
        // they are equal at compile time.
        if let (Some(l), Some(r)) = (l_c.to_const(), r_c.to_const()) {
            if l == r {
                return Expression::default();
            } else {
                return Expression::one();
            }
        }
        let mut x = InternalVar::from(constraints::subtract(
            l_c.expression(),
            FieldElement::one(),
            r_c.expression(),
        ));
        //todo we need a witness because of the directive, but we should use an expression
        let x_witness =
            x.get_or_compute_witness(evaluator, false).expect("unexpected constant expression");
        expression_from_witness(constraints::evaluate_zero_equality(x_witness, evaluator))
    }

    fn evaluate_eq(
        &mut self,
        lhs: NodeId,
        rhs: NodeId,
        l_c: Option<InternalVar>,
        r_c: Option<InternalVar>,
        ctx: &SsaContext,
        evaluator: &mut Evaluator,
    ) -> Expression {
        let neq = self.evaluate_neq(lhs, rhs, l_c, r_c, ctx, evaluator);
        constraints::subtract(&Expression::one(), FieldElement::one(), &neq)
    }

    // Given two `MemArray`s, generate constraints that check whether
    // these two arrays are equal. An `Expression` is returned representing
    // `0` if the arrays were equal and `1` otherwise.
    //
    // N.B. We assumes the lengths of a and b are the same but it is not checked inside the function.
    fn array_eq(&mut self, a: &MemArray, b: &MemArray, evaluator: &mut Evaluator) -> Expression {
        // Fetch the elements in both `MemArrays`s, these are `InternalVar`s
        // We then convert these to `Expressions`
        let internal_var_to_expr = |internal_var: InternalVar| internal_var.expression().clone();
        let a_values = vecmap(self.memory_map.load_array(a), internal_var_to_expr);
        let b_values = vecmap(self.memory_map.load_array(b), internal_var_to_expr);

        constraints::arrays_eq_predicate(&a_values, &b_values, evaluator)
    }

    // Generate constraints for two types of functions:
    // - Builtin functions: These are functions that
    // are implemented by the compiler.
    // - ACIR black box functions. These are referred
    // to as `LowLevel`
    fn evaluate_opcode(
        &mut self,
        instruction_id: NodeId,
        opcode: builtin::Opcode,
        args: &[NodeId],
        res_type: ObjectType,
        ctx: &SsaContext,
        evaluator: &mut Evaluator,
    ) -> Expression {
        use builtin::Opcode;

        let outputs;
        match opcode {
            Opcode::ToBits => {
                // TODO: document where `0` and `1` are coming from, for args[0], args[1]
                let bit_size = ctx.get_as_constant(args[1]).unwrap().to_u128() as u32;
                let l_c = self.node_id_to_internal_var_unwrap(args[0], evaluator, ctx);
                outputs = to_radix_base(l_c.expression(), 2, bit_size, evaluator);
                if let ObjectType::Pointer(a) = res_type {
                    self.memory_map.map_array(a, &outputs, ctx);
                }
            }
            Opcode::ToRadix => {
                // TODO: document where `0`, `1` and `2` are coming from, for args[0],args[1], args[2]
                let radix = ctx.get_as_constant(args[1]).unwrap().to_u128() as u32;
                let limb_size = ctx.get_as_constant(args[2]).unwrap().to_u128() as u32;
                let l_c = self.node_id_to_internal_var_unwrap(args[0], evaluator, ctx);
                outputs = to_radix_base(l_c.expression(), radix, limb_size, evaluator);
                if let ObjectType::Pointer(a) = res_type {
                    self.memory_map.map_array(a, &outputs, ctx);
                }
            }
            Opcode::LowLevel(op) => {
                let inputs = intrinsics::prepare_inputs(
                    &mut self.arith_cache,
                    &mut self.memory_map,
                    args,
                    ctx,
                    evaluator,
                );
                let output_count = op.definition().output_size.0 as u32;
                outputs = intrinsics::prepare_outputs(
                    &mut self.memory_map,
                    instruction_id,
                    output_count,
                    ctx,
                    evaluator,
                );

                let func_call = BlackBoxFuncCall {
                    name: op,
                    inputs,                   //witness + bit size
                    outputs: outputs.clone(), //witness
                };
                evaluator.opcodes.push(AcirOpcode::BlackBoxFuncCall(func_call));
            }
        }
        // TODO: document why we only return something when outputs.len()==1
        // TODO what about outputs.len() > 1
        if outputs.len() == 1 {
            expression_from_witness(outputs[0])
        } else {
            //if there are more than one witness returned, the result is inside ins.res_type as a pointer to an array
            // TODO: this is representing no expression
            Expression::default()
        }
    }
}

fn simplify_bitwise(
    lhs: &InternalVar,
    rhs: &InternalVar,
    bit_size: u32,
    opcode: &BinaryOp,
) -> Option<InternalVar> {
    // Simplifies Bitwise operations of the form `a OP a`
    // where `a` is an integer
    //
    // a XOR a == 0
    // a AND a == a
    // a OR  a == a
    if lhs == rhs {
        return Some(match opcode {
            BinaryOp::And => lhs.clone(),
            BinaryOp::Or => lhs.clone(),
            BinaryOp::Xor => InternalVar::from(FieldElement::zero()),
            _ => unreachable!("This method should only be called on bitwise binary operators"),
        });
    }

    assert!(bit_size < FieldElement::max_num_bits());
    let max = FieldElement::from((1_u128 << bit_size) - 1);
    let mut field = None;
    let mut var = lhs;
    if let Some(l_c) = lhs.to_const() {
        if l_c == FieldElement::zero() || l_c == max {
            field = Some(l_c);
            var = rhs
        }
    } else if let Some(r_c) = rhs.to_const() {
        if r_c == FieldElement::zero() || r_c == max {
            field = Some(r_c);
        }
    }
    if let Some(field) = field {
        //simplify bitwise operation of the form: 0 OP var or 1 OP var
        return Some(match opcode {
            BinaryOp::And => {
                if field.is_zero() {
                    InternalVar::from(field)
                } else {
                    var.clone()
                }
            }
            BinaryOp::Xor => {
                if field.is_zero() {
                    var.clone()
                } else {
                    InternalVar::from(constraints::subtract(
                        &Expression::from_field(field),
                        FieldElement::one(),
                        var.expression(),
                    ))
                }
            }
            BinaryOp::Or => {
                if field.is_zero() {
                    var.clone()
                } else {
                    InternalVar::from(field)
                }
            }
            _ => unreachable!(),
        });
    }

    None
}
// Precondition: `lhs` and `rhs` do not represent constant expressions
fn evaluate_bitwise(
    mut lhs: InternalVar,
    mut rhs: InternalVar,
    bit_size: u32,
    evaluator: &mut Evaluator,
    opcode: BinaryOp,
) -> Expression {
    // Check precondition
    if let (Some(_), Some(_)) = (lhs.to_const(), rhs.to_const()) {
        unreachable!("ICE: `lhs` and `rhs` are expected to be simplified. Therefore it should not be possible for both to be constants.");
    }

    if bit_size == 1 {
        match opcode {
            BinaryOp::And => {
                return constraints::mul_with_witness(evaluator, lhs.expression(), rhs.expression())
            }
            BinaryOp::Xor => {
                let sum = constraints::add(lhs.expression(), FieldElement::one(), rhs.expression());
                let mul =
                    constraints::mul_with_witness(evaluator, lhs.expression(), rhs.expression());
                return constraints::subtract(&sum, FieldElement::from(2_i128), &mul);
            }
            BinaryOp::Or => {
                let sum = constraints::add(lhs.expression(), FieldElement::one(), rhs.expression());
                let mul =
                    constraints::mul_with_witness(evaluator, lhs.expression(), rhs.expression());
                return constraints::subtract(&sum, FieldElement::one(), &mul);
            }
            _ => unreachable!(),
        }
    }
    //We generate witness from const values in order to use the ACIR bitwise gates
    // If the gate is implemented, it is expected to be better than going through bit decomposition, even if one of the operand is a constant
    // If the gate is not implemented, we rely on the ACIR simplification to remove these witnesses
    //

    let mut a_witness = lhs
        .get_or_compute_witness(evaluator, true)
        .expect("infallible: `None` can only be returned when we disallow constant Expressions.");
    let mut b_witness = rhs
        .get_or_compute_witness(evaluator, true)
        .expect("infallible: `None` can only be returned when we disallow constant Expressions.");

    let result = evaluator.add_witness_to_cs();
    let bit_size = if bit_size % 2 == 1 { bit_size + 1 } else { bit_size };
    assert!(bit_size < FieldElement::max_num_bits() - 1);
    let max = FieldElement::from((1_u128 << bit_size) - 1);
    let bit_gate = match opcode {
        BinaryOp::And => acvm::acir::BlackBoxFunc::AND,
        BinaryOp::Xor => acvm::acir::BlackBoxFunc::XOR,
        BinaryOp::Or => {
            a_witness = evaluator.create_intermediate_variable(constraints::subtract(
                &Expression::from_field(max),
                FieldElement::one(),
                lhs.expression(),
            ));
            b_witness = evaluator.create_intermediate_variable(constraints::subtract(
                &Expression::from_field(max),
                FieldElement::one(),
                rhs.expression(),
            ));
            // We do not have an OR gate yet, so we use the AND gate
            acvm::acir::BlackBoxFunc::AND
        }
        _ => unreachable!("ICE: expected a bitwise operation"),
    };

    let gate = AcirOpcode::BlackBoxFuncCall(BlackBoxFuncCall {
        name: bit_gate,
        inputs: vec![
            FunctionInput { witness: a_witness, num_bits: bit_size },
            FunctionInput { witness: b_witness, num_bits: bit_size },
        ],
        outputs: vec![result],
    });
    evaluator.opcodes.push(gate);

    if opcode == BinaryOp::Or {
        constraints::subtract(
            &Expression::from_field(max),
            FieldElement::one(),
            &expression_from_witness(result),
        )
    } else {
        expression_from_witness(result)
    }
}

// Creates an Expression from a Witness.
//
// This is infallible since an expression is
// a multi-variate polynomial and a Witness
// can be seen as a univariate polynomial
//
// TODO: Possibly remove this small shim.
// TODO: Lets first see how the rest of the code looks after
// TODO further refactor.
fn expression_from_witness(witness: Witness) -> Expression {
    Expression::from(&witness)
}

/// Returns a `FieldElement` if the expression represents
/// a constant polynomial
///
// TODO we should have a method in ACVM
// TODO which returns the constant term if its a constant
// TODO expression. ie `self.expression.to_const()`
fn const_from_expression(expression: &Expression) -> Option<FieldElement> {
    expression.is_const().then_some(expression.q_c)
}

// Returns a `Witness` if the `Expression` can be represented as a degree-1
// univariate polynomial. Otherwise, Return None.
//
// Note that `Witness` is only capable of expressing polynomials of the form
// f(x) = x and not polynomials of the form f(x) = mx+c , so this method has
// extra checks to ensure that m=1 and c=0
//
// TODO: move to ACVM repo
fn optional_expression_to_witness(arith: &Expression) -> Option<Witness> {
    let is_deg_one_univariate = expression_is_deg_one_univariate(arith);

    if is_deg_one_univariate {
        // If we get here, we know that our expression is of the form `f(x) = mx+c`
        // We want to now restrict ourselves to expressions of the form f(x) = x
        // ie where the constant term is 0 and the coefficient in front of the variable is
        // one.
        let coefficient = arith.linear_combinations[0].0;
        let variable = arith.linear_combinations[0].1;
        let constant = arith.q_c;

        let coefficient_is_one = coefficient.is_one();
        let constant_term_is_zero = constant.is_zero();

        if coefficient_is_one && constant_term_is_zero {
            return Some(variable);
        }
    }

    None
}
/// Converts an `Expression` into a `Witness`
/// - If the `Expression` is a degree-1 univariate polynomial
/// then this conversion is a simple coercion.
/// - Otherwise, we create a new `Witness` and set it to be equal to the
/// `Expression`.
pub(crate) fn expression_to_witness<A: constraints::ACIRState>(
    expr: Expression,
    evaluator: &mut A,
) -> Witness {
    match optional_expression_to_witness(&expr) {
        Some(witness) => witness,
        None => evaluator.create_intermediate_variable(expr),
    }
}
// Returns true if highest degree term in the expression is one.
//
// - `mul_term` in an expression contains degree-2 terms
// - `linear_combinations` contains degree-1 terms
// Hence, it is sufficient to check that there are no `mul_terms`
//
// Examples:
// -  f(x,y) = x + y would return true
// -  f(x,y) = xy would return false, the degree here is 2
// -  f(x,y) = 0 would return true, the degree is 0
//
// TODO: move to ACVM repo
fn expression_is_degree_1(expression: &Expression) -> bool {
    expression.mul_terms.is_empty()
}
// Returns true if the expression can be seen as a degree-1 univariate polynomial
//
// - `mul_terms` in an expression can be univariate, however unless the coefficient
// is zero, it is always degree-2.
// - `linear_combinations` contains the sum of degree-1 terms, these terms do not
// need to contain the same variable and so it can be multivariate. However, we
// have thus far only checked if `linear_combinations` contains one term, so this
// method will return false, if the `Expression` has not been simplified.
//
// Hence, we check in the simplest case if an expression is a degree-1 univariate,
// by checking if it contains no `mul_terms` and it contains one `linear_combination` term.
//
// Examples:
// - f(x,y) = x would return true
// - f(x,y) = x + 6 would return true
// - f(x,y) = 2*y + 6 would return true
// - f(x,y) = x + y would return false
// - f(x,y) = x + x should return true, but we return false *** (we do not simplify)
// - f(x,y) = 5 would return false
// TODO move to ACVM repo
// TODO: ACVM has a method called is_linear, we should change this to `max_degree_one`
fn expression_is_deg_one_univariate(expression: &Expression) -> bool {
    let has_one_univariate_term = expression.linear_combinations.len() == 1;
    expression_is_degree_1(expression) && has_one_univariate_term
}
