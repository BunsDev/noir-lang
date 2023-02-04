use crate::{
    errors::RuntimeErrorKind,
    ssa::acir_gen::{bit_size_u128, expression_from_witness, InternalVar},
    Evaluator,
};
use acvm::{
    acir::{
        circuit::{
            directives::Directive,
            opcodes::{BlackBoxFuncCall, FunctionInput, Opcode as AcirOpcode},
        },
        native_types::{Expression, Witness},
    },
    FieldElement,
};
use std::{cmp::Ordering, ops::Neg};

// Code in this file, will generate constraints without
// using only the Evaluator and ACIR Expression types

pub(crate) fn mul_with_witness(
    evaluator: &mut Evaluator,
    a: &Expression,
    b: &Expression,
) -> Expression {
    let a_arith;
    let a_arith = if !a.mul_terms.is_empty() && !b.is_const() {
        let a_witness = evaluator.add_witness_to_cs();
        a_arith = Expression::from(&a_witness);
        evaluator.opcodes.push(AcirOpcode::Arithmetic(a - &a_arith));
        &a_arith
    } else {
        a
    };
    let b_arith;
    let b_arith = if !b.mul_terms.is_empty() && !a.is_const() {
        if a == b {
            a_arith
        } else {
            let b_witness = evaluator.add_witness_to_cs();
            b_arith = Expression::from(&b_witness);
            evaluator.opcodes.push(AcirOpcode::Arithmetic(b - &b_arith));
            &b_arith
        }
    } else {
        b
    };
    mul(a_arith, b_arith)
}

//a*b
pub(crate) fn mul(a: &Expression, b: &Expression) -> Expression {
    let zero = Expression::zero();
    if a.is_const() {
        return add(&zero, a.q_c, b);
    } else if b.is_const() {
        return add(&zero, b.q_c, a);
    } else if !(a.is_linear() && b.is_linear()) {
        unreachable!("Can only multiply linear terms");
    }

    let mut output = Expression::from_field(a.q_c * b.q_c);

    //TODO to optimize...
    for lc in &a.linear_combinations {
        let single = single_mul(lc.1, b);
        output = add(&output, lc.0, &single);
    }

    //linear terms
    let mut i1 = 0; //a
    let mut i2 = 0; //b
    while i1 < a.linear_combinations.len() && i2 < b.linear_combinations.len() {
        let coeff_a = b.q_c * a.linear_combinations[i1].0;
        let coeff_b = a.q_c * b.linear_combinations[i2].0;
        match a.linear_combinations[i1].1.cmp(&b.linear_combinations[i2].1) {
            Ordering::Greater => {
                if coeff_b != FieldElement::zero() {
                    output.linear_combinations.push((coeff_b, b.linear_combinations[i2].1));
                }
                i2 += 1;
            }
            Ordering::Less => {
                if coeff_a != FieldElement::zero() {
                    output.linear_combinations.push((coeff_a, a.linear_combinations[i1].1));
                }
                i1 += 1;
            }
            Ordering::Equal => {
                if coeff_a + coeff_b != FieldElement::zero() {
                    output
                        .linear_combinations
                        .push((coeff_a + coeff_b, a.linear_combinations[i1].1));
                }

                i1 += 1;
                i2 += 1;
            }
        }
    }
    while i1 < a.linear_combinations.len() {
        let coeff_a = b.q_c * a.linear_combinations[i1].0;
        output.linear_combinations.push((coeff_a, a.linear_combinations[i1].1));
        i1 += 1;
    }
    while i2 < b.linear_combinations.len() {
        let coeff_b = a.q_c * b.linear_combinations[i2].0;
        output.linear_combinations.push((coeff_b, b.linear_combinations[i2].1));
        i2 += 1;
    }

    output
}

// returns a - k*b
pub(crate) fn subtract(a: &Expression, k: FieldElement, b: &Expression) -> Expression {
    add(a, k.neg(), b)
}

// returns a + k*b
pub(crate) fn add(a: &Expression, k: FieldElement, b: &Expression) -> Expression {
    let mut output = Expression::default();

    //linear combinations
    let mut i1 = 0; //a
    let mut i2 = 0; //b
    while i1 < a.linear_combinations.len() && i2 < b.linear_combinations.len() {
        match a.linear_combinations[i1].1.cmp(&b.linear_combinations[i2].1) {
            Ordering::Greater => {
                let coeff = b.linear_combinations[i2].0 * k;
                if coeff != FieldElement::zero() {
                    output.linear_combinations.push((coeff, b.linear_combinations[i2].1));
                }
                i2 += 1;
            }
            Ordering::Less => {
                output.linear_combinations.push(a.linear_combinations[i1]);
                i1 += 1;
            }
            Ordering::Equal => {
                let coeff = a.linear_combinations[i1].0 + b.linear_combinations[i2].0 * k;
                if coeff != FieldElement::zero() {
                    output.linear_combinations.push((coeff, a.linear_combinations[i1].1));
                }
                i2 += 1;
                i1 += 1;
            }
        }
    }
    while i1 < a.linear_combinations.len() {
        output.linear_combinations.push(a.linear_combinations[i1]);
        i1 += 1;
    }
    while i2 < b.linear_combinations.len() {
        let coeff = b.linear_combinations[i2].0 * k;
        if coeff != FieldElement::zero() {
            output.linear_combinations.push((coeff, b.linear_combinations[i2].1));
        }
        i2 += 1;
    }

    //mul terms

    i1 = 0; //a
    i2 = 0; //b

    while i1 < a.mul_terms.len() && i2 < b.mul_terms.len() {
        match (a.mul_terms[i1].1, a.mul_terms[i1].2).cmp(&(b.mul_terms[i2].1, b.mul_terms[i2].2)) {
            Ordering::Greater => {
                let coeff = b.mul_terms[i2].0 * k;
                if coeff != FieldElement::zero() {
                    output.mul_terms.push((coeff, b.mul_terms[i2].1, b.mul_terms[i2].2));
                }
                i2 += 1;
            }
            Ordering::Less => {
                output.mul_terms.push(a.mul_terms[i1]);
                i1 += 1;
            }
            Ordering::Equal => {
                let coeff = a.mul_terms[i1].0 + b.mul_terms[i2].0 * k;
                if coeff != FieldElement::zero() {
                    output.mul_terms.push((coeff, a.mul_terms[i1].1, a.mul_terms[i1].2));
                }
                i2 += 1;
                i1 += 1;
            }
        }
    }
    while i1 < a.mul_terms.len() {
        output.mul_terms.push(a.mul_terms[i1]);
        i1 += 1;
    }

    while i2 < b.mul_terms.len() {
        let coeff = b.mul_terms[i2].0 * k;
        if coeff != FieldElement::zero() {
            output.mul_terms.push((coeff, b.mul_terms[i2].1, b.mul_terms[i2].2));
        }
        i2 += 1;
    }

    output.q_c = a.q_c + k * b.q_c;
    output
}

// returns w*b.linear_combinations
pub(crate) fn single_mul(w: Witness, b: &Expression) -> Expression {
    let mut output = Expression::default();
    let mut i1 = 0;
    while i1 < b.linear_combinations.len() {
        if (w, b.linear_combinations[i1].1) < (b.linear_combinations[i1].1, w) {
            output.mul_terms.push((b.linear_combinations[i1].0, w, b.linear_combinations[i1].1));
        } else {
            output.mul_terms.push((b.linear_combinations[i1].0, b.linear_combinations[i1].1, w));
        }
        i1 += 1;
    }
    output
}

pub(crate) fn boolean(witness: Witness) -> Expression {
    Expression {
        mul_terms: vec![(FieldElement::one(), witness, witness)],
        linear_combinations: vec![(-FieldElement::one(), witness)],
        q_c: FieldElement::zero(),
    }
}

pub(crate) fn boolean_expr(expr: &Expression, evaluator: &mut Evaluator) -> Expression {
    subtract(&mul_with_witness(evaluator, expr, expr), FieldElement::one(), expr)
}

//constrain witness a to be num_bits-size integer, i.e between 0 and 2^num_bits-1
pub fn range_constraint(
    witness: Witness,
    num_bits: u32,
    evaluator: &mut Evaluator,
) -> Result<(), RuntimeErrorKind> {
    if num_bits == 1 {
        // Add a bool gate
        let bool_constraint = boolean(witness);
        evaluator.opcodes.push(AcirOpcode::Arithmetic(bool_constraint));
    } else if num_bits == FieldElement::max_num_bits() {
        // Don't apply any constraints if the range is for the maximum number of bits
        let message = format!(
            "All Witnesses are by default u{}. Applying this type does not apply any constraints.",
            FieldElement::max_num_bits()
        );
        return Err(RuntimeErrorKind::UnstructuredError { message });
    } else if num_bits % 2 == 1 {
        // Note if the number of bits is odd, then Barretenberg will panic
        // new witnesses; r is constrained to num_bits-1 and b is 1 bit
        let r_witness = evaluator.add_witness_to_cs();
        let b_witness = evaluator.add_witness_to_cs();
        evaluator.opcodes.push(AcirOpcode::Directive(Directive::OddRange {
            a: witness,
            b: b_witness,
            r: r_witness,
            bit_size: num_bits,
        }));

        try_range_constraint(r_witness, num_bits - 1, evaluator);
        try_range_constraint(b_witness, 1, evaluator);

        //Add the constraint a = r + 2^N*b
        let mut f = FieldElement::from(2_i128);
        f = f.pow(&FieldElement::from((num_bits - 1) as i128));
        let res = add(&expression_from_witness(r_witness), f, &expression_from_witness(b_witness));
        let my_constraint = add(&res, -FieldElement::one(), &expression_from_witness(witness));
        evaluator.opcodes.push(AcirOpcode::Arithmetic(my_constraint));
    } else {
        let gate = AcirOpcode::BlackBoxFuncCall(BlackBoxFuncCall {
            name: acvm::acir::BlackBoxFunc::RANGE,
            inputs: vec![FunctionInput { witness, num_bits }],
            outputs: vec![],
        });
        evaluator.opcodes.push(gate);
    }

    Ok(())
}

// returns a witness of a>=b
pub(crate) fn bound_check(
    a: &Expression,
    b: &Expression,
    max_bits: u32,
    evaluator: &mut Evaluator,
) -> Witness {
    assert!(max_bits + 1 < FieldElement::max_num_bits()); //n.b what we really need is 2^{max_bits+1}<p
    let mut sub = subtract(a, FieldElement::one(), b);
    let two = FieldElement::from(2_i128);
    let two_s = two.pow(&FieldElement::from(max_bits as i128));
    sub.q_c += two_s;
    let q_witness = evaluator.add_witness_to_cs();
    let r_witness = evaluator.add_witness_to_cs();
    //2^s+a-b=q*2^s +r
    let expr = add(&expression_from_witness(r_witness), two_s, &expression_from_witness(q_witness));
    evaluator.opcodes.push(AcirOpcode::Arithmetic(subtract(&sub, FieldElement::one(), &expr)));
    evaluator.opcodes.push(AcirOpcode::Directive(Directive::Truncate {
        a: sub,
        b: r_witness,
        c: q_witness,
        bit_size: max_bits,
    }));
    try_range_constraint(r_witness, max_bits, evaluator);
    evaluator.opcodes.push(AcirOpcode::Arithmetic(boolean(q_witness)));
    q_witness
}

// Generate constraints that are satisfied iff
// a < b , when offset is 1, or
// a <= b, when offset is 0
// bits is the bit size of a and b (or an upper bound of the bit size)
///////////////
// a<=b is done by constraining b-a to a bit size of 'bits':
// if a<=b, 0 <= b-a <= b < 2^bits
// if a>b, b-a = p+b-a > p-2^bits >= 2^bits  (if log(p) >= bits + 1)
// n.b: we do NOT check here that a and b are indeed 'bits' size
// a < b <=> a+1<=b
pub(crate) fn bound_constraint_with_offset(
    a: &Expression,
    b: &Expression,
    offset: &Expression,
    bits: u32,
    evaluator: &mut Evaluator,
) {
    assert!(
        bits < FieldElement::max_num_bits(),
        "range check with bit size of the prime field is not implemented yet"
    );

    let mut aof = add(a, FieldElement::one(), offset);

    if b.is_const() && b.q_c.fits_in_u128() {
        let f = if *offset == Expression::one() {
            aof = a.clone();
            assert!(b.q_c.to_u128() >= 1);
            b.q_c.to_u128() - 1
        } else {
            b.q_c.to_u128()
        };

        if f < 3 {
            match f {
                0 => evaluator.opcodes.push(AcirOpcode::Arithmetic(aof)),
                1 => {
                    let expr = boolean_expr(&aof, evaluator);
                    evaluator.opcodes.push(AcirOpcode::Arithmetic(expr))
                }
                2 => {
                    let y = InternalVar::expression_to_witness(
                        boolean_expr(&aof, evaluator),
                        evaluator,
                    );
                    let two = FieldElement::from(2_i128);
                    let y_expr = expression_from_witness(y);
                    let eee = subtract(&mul_with_witness(evaluator, &aof, &y_expr), two, &y_expr);
                    evaluator.opcodes.push(AcirOpcode::Arithmetic(eee));
                }
                _ => unreachable!(),
            }
            return;
        }
        let bit_size = bit_size_u128(f);
        if bit_size < 128 {
            let r = (1_u128 << bit_size) - f - 1;
            assert!(bits + bit_size < FieldElement::max_num_bits()); //we need to ensure a+r does not overflow
            let aor = add(&aof, FieldElement::from(r), &Expression::one());
            let witness = InternalVar::expression_to_witness(aor, evaluator);
            try_range_constraint(witness, bit_size, evaluator);
            return;
        }
    }

    let sub_expression = subtract(b, FieldElement::one(), &aof); //b-(a+offset)
    let w = InternalVar::expression_to_witness(sub_expression, evaluator);
    try_range_constraint(w, bits, evaluator);
}

pub(crate) fn try_range_constraint(w: Witness, bits: u32, evaluator: &mut Evaluator) {
    if let Err(err) = range_constraint(w, bits, evaluator) {
        eprintln!("{err}");
    }
}
