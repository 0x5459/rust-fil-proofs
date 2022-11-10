use ff::{PrimeField, PrimeFieldBits};
use fil_halo2_gadgets::boolean::{lebs2ip, Bit};
use halo2_gadgets::utilities::bool_check;
use halo2_gadgets::utilities::decompose_running_sum::{RunningSum, RunningSumConfig};
use halo2_proofs::circuit::{AssignedCell, Cell, Layouter, Region, SimpleFloorPlanner, Value};
use halo2_proofs::dev::MockProver;
use halo2_proofs::pasta::{EqAffine, Fp};
use halo2_proofs::plonk::{
    create_proof, keygen_pk, keygen_vk, verify_proof, Advice, Circuit, Column, ConstraintSystem,
    Constraints, Error, Expression, Instance, Selector, SingleVerifier, VirtualCells,
};
use halo2_proofs::poly::commitment::Params;
use halo2_proofs::poly::Rotation;
use halo2_proofs::transcript::{Blake2bRead, Blake2bWrite, Challenge255};
use rand::rngs::OsRng;
use rand::Rng;
use std::convert::TryInto;

const WORD_BIT_LENGTH: usize = 32;
const WINDOW_BIT_LENGTH: usize = 3;
const WINDOWS_NUMBER: usize = (WORD_BIT_LENGTH + WINDOW_BIT_LENGTH - 1) / WINDOW_BIT_LENGTH;

#[derive(Clone)]
struct RangeCheckConfig {
    running_sum: RunningSumConfig<Fp, WINDOW_BIT_LENGTH>,
}

impl RangeCheckConfig {
    fn witness_decompose(
        &self,
        region: &mut Region<'_, Fp>,
        offset: usize,
        alpha: Value<Fp>,
        strict: bool,
        word_num_bits: usize,
        num_windows: usize,
    ) -> Result<RunningSum<Fp>, Error> {
        self.running_sum.witness_decompose(
            region,
            offset,
            alpha,
            strict,
            word_num_bits,
            num_windows,
        )
    }

    fn copy_decompose(
        &self,
        region: &mut Region<'_, Fp>,
        offset: usize,
        alpha: AssignedCell<Fp, Fp>,
        strict: bool,
        word_num_bits: usize,
        num_windows: usize,
    ) -> Result<RunningSum<Fp>, Error> {
        self.running_sum
            .copy_decompose(region, offset, alpha, strict, word_num_bits, num_windows)
    }
}

struct RangeCheckChip {
    config: RangeCheckConfig,
}

impl RangeCheckChip {
    fn construct(config: RangeCheckConfig) -> Self {
        RangeCheckChip { config }
    }

    fn configure(meta: &mut ConstraintSystem<Fp>) -> RangeCheckConfig {
        let z = meta.advice_column();
        let q_range_check = meta.selector();
        let constants = meta.fixed_column();
        meta.enable_constant(constants);

        let running_sum =
            RunningSumConfig::<Fp, WINDOW_BIT_LENGTH>::configure(meta, q_range_check, z);

        RangeCheckConfig { running_sum }
    }

    fn range_check(
        &self,
        mut layouter: impl Layouter<Fp>,
        a: Value<Fp>,
    ) -> Result<AssignedCell<Fp, Fp>, Error> {
        layouter.assign_region(
            || "range check",
            |mut region| {
                let offset = 0;
                let zs = self.config.witness_decompose(
                    &mut region,
                    offset,
                    a,
                    true,
                    WORD_BIT_LENGTH,
                    WINDOWS_NUMBER,
                )?;

                let b = zs[0].clone();

                let offset = offset + WINDOWS_NUMBER + 1;

                let running_sum = self.config.copy_decompose(
                    &mut region,
                    offset,
                    b,
                    true,
                    WORD_BIT_LENGTH,
                    WINDOWS_NUMBER,
                )?;

                Ok(running_sum[0].clone())
            },
        )
    }
}

#[derive(Clone)]
struct AssignFp32BitsConfig {
    bits_column: Column<Advice>,
    bits_assignment_selector: Selector,
}

struct AssignFp32BitsChip {
    config: AssignFp32BitsConfig,
}

impl AssignFp32BitsChip {
    fn construct(config: AssignFp32BitsConfig) -> Self {
        AssignFp32BitsChip { config }
    }
    fn configure(meta: &mut ConstraintSystem<Fp>) -> AssignFp32BitsConfig {
        let bits_column = meta.advice_column();
        let bits_assignment_selector = meta.selector();
        meta.enable_equality(bits_column);

        meta.create_gate("boolean constraint", |meta: &mut VirtualCells<Fp>| {
            let selector = meta.query_selector(bits_assignment_selector);
            let bit = meta.query_advice(bits_column, Rotation::cur());

            Constraints::with_selector(
                selector,
                vec![("a is boolean", bool_check(bit))].into_iter(),
            )
        });

        AssignFp32BitsConfig {
            bits_column,
            bits_assignment_selector,
        }
    }

    fn assign_bit(
        &self,
        region: &mut Region<Fp>,
        bit: Bit,
        offset: usize,
    ) -> Result<AssignedCell<Bit, Fp>, Error> {
        self.config
            .bits_assignment_selector
            .enable(region, offset)?;
        region.assign_advice(
            || format!("bit assignment {}", offset),
            self.config.bits_column,
            offset,
            || Value::known(bit),
        )
    }

    fn assign_32bits_of_fp(
        &self,
        mut layouter: impl Layouter<Fp>,
        fp: Value<Fp>,
    ) -> Result<Vec<AssignedCell<Bit, Fp>>, Error> {
        let assigned_bits: Vec<AssignedCell<Bit, Fp>> = layouter.assign_region(
            || "assign 32 first (little endian) bits of fp",
            |mut region| {
                let mut bits: Vec<bool> = vec![];
                fp.map(|fp| {
                    // collect only 32-bit word contained in Fp
                    for (index, bit) in fp.to_le_bits().into_iter().enumerate() {
                        if index < 32 {
                            bits.push(bit)
                        }
                    }
                });
                bits.into_iter()
                    .enumerate()
                    .map(|(index, bit)| self.assign_bit(&mut region, Bit::from(bit), index))
                    .collect()
            },
        )?;

        Ok(assigned_bits)
    }
}

#[derive(Clone)]
struct PackChipConfig {
    fp: Column<Advice>,
    bits: Column<Advice>,
    selector: Selector,
}

struct PackChip {
    config: PackChipConfig,
}

impl PackChip {
    fn construct(config: PackChipConfig) -> Self {
        PackChip { config }
    }

    fn check_packing(
        &self,
        mut layouter: impl Layouter<Fp>,
        fp: AssignedCell<Fp, Fp>,
        decomposed: Vec<AssignedCell<Bit, Fp>>,
    ) -> Result<Vec<AssignedCell<Bit, Fp>>, Error> {
        layouter.assign_region(
            || "pack",
            |mut region| {
                self.config.selector.enable(&mut region, 0)?;

                fp.copy_advice(|| "fp copy", &mut region, self.config.fp, 0)?;

                let bits = decomposed
                    .iter()
                    .enumerate()
                    .map(|(index, bit)| {
                        bit.copy_advice(
                            || format!("bit {} copy", index),
                            &mut region,
                            self.config.bits,
                            index,
                        )
                    })
                    .collect();
                bits
            },
        )
    }

    fn configure(meta: &mut ConstraintSystem<Fp>) -> PackChipConfig {
        let fp = meta.advice_column();
        let bits = meta.advice_column();
        let selector = meta.selector();

        meta.enable_equality(fp);
        meta.enable_equality(bits);

        meta.create_gate("pack", |meta| {
            let selector = meta.query_selector(selector);

            let bits = (0..32)
                .map(|index| meta.query_advice(bits, Rotation(index)))
                .collect::<Vec<Expression<Fp>>>();

            let constant_expressions = (0..32)
                .map(|degree_of_two| {
                    Expression::Constant(Fp::from(2_u64.pow(degree_of_two) as u64))
                })
                .collect::<Vec<Expression<Fp>>>();

            let fp = meta.query_advice(fp, Rotation::cur());

            Constraints::with_selector(
                selector,
                vec![(
                    "pack",
                    bits[0].clone() * constant_expressions[0].clone()
                        + bits[1].clone() * constant_expressions[1].clone()
                        + bits[2].clone() * constant_expressions[2].clone()
                        + bits[3].clone() * constant_expressions[3].clone()
                        + bits[4].clone() * constant_expressions[4].clone()
                        + bits[5].clone() * constant_expressions[5].clone()
                        + bits[6].clone() * constant_expressions[6].clone()
                        + bits[7].clone() * constant_expressions[7].clone()
                        + bits[8].clone() * constant_expressions[8].clone()
                        + bits[9].clone() * constant_expressions[9].clone()
                        + bits[10].clone() * constant_expressions[10].clone()
                        + bits[11].clone() * constant_expressions[11].clone()
                        + bits[12].clone() * constant_expressions[12].clone()
                        + bits[13].clone() * constant_expressions[13].clone()
                        + bits[14].clone() * constant_expressions[14].clone()
                        + bits[15].clone() * constant_expressions[15].clone()
                        + bits[16].clone() * constant_expressions[16].clone()
                        + bits[17].clone() * constant_expressions[17].clone()
                        + bits[18].clone() * constant_expressions[18].clone()
                        + bits[19].clone() * constant_expressions[19].clone()
                        + bits[20].clone() * constant_expressions[20].clone()
                        + bits[21].clone() * constant_expressions[21].clone()
                        + bits[22].clone() * constant_expressions[22].clone()
                        + bits[23].clone() * constant_expressions[23].clone()
                        + bits[24].clone() * constant_expressions[24].clone()
                        + bits[25].clone() * constant_expressions[25].clone()
                        + bits[26].clone() * constant_expressions[26].clone()
                        + bits[27].clone() * constant_expressions[27].clone()
                        + bits[28].clone() * constant_expressions[28].clone()
                        + bits[29].clone() * constant_expressions[29].clone()
                        + bits[30].clone() * constant_expressions[30].clone()
                        + bits[31].clone() * constant_expressions[31].clone()
                        - fp,
                )]
                .into_iter(),
            )
        });

        PackChipConfig { fp, bits, selector }
    }
}

#[derive(Debug, Clone)]
struct BooleanXorConfig {
    a: Column<Advice>,
    b: Column<Advice>,
    selector: Selector,
    xor_result: Column<Advice>,
    xor_result_pi: Column<Instance>,
}

struct BooleanXorChip {
    config: BooleanXorConfig,
}

impl BooleanXorChip {
    fn construct(config: BooleanXorConfig) -> Self {
        BooleanXorChip { config }
    }
    fn configure(meta: &mut ConstraintSystem<Fp>) -> BooleanXorConfig {
        let a = meta.advice_column();
        let b = meta.advice_column();
        let xor_result = meta.advice_column();
        let xor_result_pi = meta.instance_column();
        let selector = meta.selector();

        meta.enable_equality(a);
        meta.enable_equality(b);
        meta.enable_equality(xor_result);
        meta.enable_equality(xor_result_pi);

        meta.create_gate("xor", |meta: &mut VirtualCells<Fp>| {
            let selector = meta.query_selector(selector);
            let a = meta.query_advice(a, Rotation::cur());
            let b = meta.query_advice(b, Rotation::cur());
            let out = meta.query_advice(xor_result, Rotation::cur());

            Constraints::with_selector(
                selector,
                vec![(
                    "Bitwise XOR: a - a_and_b + b - a_and_b - a_xor_b == 0",
                    (a.clone() + a.clone()) * b.clone() - a - b + out,
                )]
                .into_iter(),
            )
        });

        BooleanXorConfig {
            a,
            b,
            xor_result,
            xor_result_pi,
            selector,
        }
    }
}

trait Instructions {
    fn xor(
        &self,
        layouter: impl Layouter<Fp>,
        a: &AssignedCell<Bit, Fp>,
        b: &AssignedCell<Bit, Fp>,
        advice_offset: usize,
    ) -> Result<AssignedCell<Bit, Fp>, Error>;
    fn expose_public(
        &self,
        layouter: impl Layouter<Fp>,
        cell: Cell,
        instance_offset: usize,
    ) -> Result<(), Error>;
}

impl Instructions for BooleanXorChip {
    fn xor(
        &self,
        mut layouter: impl Layouter<Fp>,
        a: &AssignedCell<Bit, Fp>,
        b: &AssignedCell<Bit, Fp>,
        advice_offset: usize,
    ) -> Result<AssignedCell<Bit, Fp>, Error> {
        // we can't have more than 32 bits to XOR in 32-bit word
        assert!(advice_offset < 32);
        layouter.assign_region(
            || format!("xor {}", advice_offset),
            |mut region: Region<Fp>| {
                // enable selector for the XOR gate
                self.config.selector.enable(&mut region, advice_offset)?;

                // assign a into advice column
                let a = a.copy_advice(
                    || format!("copy {} bit of a constrained", advice_offset),
                    &mut region,
                    self.config.a,
                    advice_offset,
                )?;

                // assign b into advice column
                let b = b.copy_advice(
                    || format!("copy {} bit of b constrained", advice_offset),
                    &mut region,
                    self.config.b,
                    advice_offset,
                )?;

                // compute actual value...
                let xor_result = a
                    .value()
                    .zip(b.value())
                    .map(|(a, b)| Bit(bool::from(a) ^ bool::from(b)));

                // and assign it into separate advice column
                region.assign_advice(
                    || format!("xor {}", advice_offset),
                    self.config.xor_result,
                    advice_offset,
                    || xor_result,
                )
            },
        )
    }

    fn expose_public(
        &self,
        mut layouter: impl Layouter<Fp>,
        cell: Cell,
        instance_offset: usize,
    ) -> Result<(), Error> {
        // we expect some value provided as a public input to compare with computed xor result in the instance column
        layouter.constrain_instance(cell, self.config.xor_result_pi, instance_offset)
    }
}

#[derive(Default)]
struct U32WordXorCircuit {
    a: Value<Fp>,
    b: Value<Fp>,
}

impl U32WordXorCircuit {
    fn k(&self) -> u32 {
        10
    }
}

impl Circuit<Fp> for U32WordXorCircuit {
    type Config = (
        RangeCheckConfig,
        AssignFp32BitsConfig,
        PackChipConfig,
        BooleanXorConfig,
    );
    type FloorPlanner = SimpleFloorPlanner;

    fn without_witnesses(&self) -> Self {
        U32WordXorCircuit {
            a: Value::unknown(),
            b: Value::unknown(),
        }
    }

    fn configure(meta: &mut ConstraintSystem<Fp>) -> Self::Config {
        let range_check_config = RangeCheckChip::configure(meta);
        let assign_32bits_config = AssignFp32BitsChip::configure(meta);
        let pack_chip_config = PackChip::configure(meta);
        let boolean_xor_config = BooleanXorChip::configure(meta);

        (
            range_check_config,
            assign_32bits_config,
            pack_chip_config,
            boolean_xor_config,
        )
    }

    fn synthesize(
        &self,
        config: Self::Config,
        mut layouter: impl Layouter<Fp>,
    ) -> Result<(), Error> {
        let range_check_chip = RangeCheckChip::construct(config.0);
        let assign_32bits_chip = AssignFp32BitsChip::construct(config.1);
        let pack_chip = PackChip::construct(config.2);

        let assigned_fp_a =
            range_check_chip.range_check(layouter.namespace(|| "range check of a"), self.a)?;

        let assigned_fp_a_bits = assign_32bits_chip
            .assign_32bits_of_fp(layouter.namespace(|| "assign 32 bits of a"), self.a)?;

        let assigned_fp_a_bits_copied = pack_chip.check_packing(
            layouter.namespace(|| "pack a"),
            assigned_fp_a,
            assigned_fp_a_bits,
        )?;

        let assigned_fp_b =
            range_check_chip.range_check(layouter.namespace(|| "range check of b"), self.b)?;

        let assigned_fp_b_bits = assign_32bits_chip
            .assign_32bits_of_fp(layouter.namespace(|| "assign 32 bits of b"), self.b)?;

        let assigned_fp_b_bits_copied = pack_chip.check_packing(
            layouter.namespace(|| "pack b"),
            assigned_fp_b,
            assigned_fp_b_bits,
        )?;

        let boolean_xor_chip = BooleanXorChip::construct(config.3);

        let xor_result = assigned_fp_a_bits_copied
            .iter()
            .zip(assigned_fp_b_bits_copied.iter())
            .enumerate()
            .map(|(index, (a_bit, b_bit))| {
                boolean_xor_chip
                    .xor(
                        layouter.namespace(|| format!("xor {}", index)),
                        a_bit,
                        b_bit,
                        index,
                    )
                    .expect("couldn't xor")
            })
            .collect::<Vec<AssignedCell<Bit, Fp>>>();

        // for convenience let's compose 32 Fps (1 or 0) into single Fp
        let bits = xor_result
            .iter()
            .map(|xor_bit| {
                let mut bit = false;
                xor_bit.value().map(|assigned_bit| {
                    if assigned_bit.0 {
                        bit = true;
                    }
                });
                bit
            })
            .collect::<Vec<bool>>();

        let fp_composed = match &bits.try_into() {
            Ok(val) => Value::known(Fp::from(lebs2ip::<32>(val))),
            Err(_) => Value::known(Fp::from(0)),
        };

        // expose single composed Fp which holds XOR result
        let cell = layouter.assign_region(
            || "assign xor result",
            |mut region| {
                region.assign_advice(
                    || "value",
                    boolean_xor_chip.config.xor_result,
                    32, // 0..31 will be occupied by bits of fp_composed
                    || fp_composed,
                )
            },
        )?;

        boolean_xor_chip.expose_public(
            layouter.namespace(|| "exposing xor result as a field element"),
            cell.cell(),
            0,
        )?;

        Ok(())
    }
}

#[test]
fn end_to_end_test_u32_words_xor() {
    fn test(a: Fp, b: Fp, c: Fp, use_circuit_prover_for_keygen: bool) -> bool {
        let circuit = U32WordXorCircuit {
            a: Value::known(a),
            b: Value::known(b),
        };

        let public_inputs = vec![c];

        let k = circuit.k();

        let params: Params<EqAffine> = Params::new(k);

        let pk = if use_circuit_prover_for_keygen {
            let vk = keygen_vk(&params, &circuit).expect("keygen_vk should not fail");
            keygen_pk(&params, vk, &circuit).expect("keygen_pk should not fail")
        } else {
            let circuit = circuit.without_witnesses();
            let vk = keygen_vk(&params, &circuit).expect("keygen_vk should not fail");
            keygen_pk(&params, vk, &circuit).expect("keygen_pk should not fail")
        };

        let mut transcript = Blake2bWrite::<_, EqAffine, Challenge255<_>>::init(vec![]);

        // Create a proof
        create_proof(
            &params,
            &pk,
            &[circuit],
            &[&[&public_inputs[..]]],
            OsRng,
            &mut transcript,
        )
        .expect("proof generation should not fail");
        let proof: Vec<u8> = transcript.finalize();

        let strategy = SingleVerifier::new(&params);
        let mut transcript = Blake2bRead::<_, _, Challenge255<_>>::init(&proof[..]);

        let result = verify_proof(
            &params,
            pk.get_vk(),
            strategy,
            &[&[&public_inputs[..]]],
            &mut transcript,
        )
        .is_ok();

        result
    }

    fn negative_test(a: Fp, b: Fp, c: Fp, use_circuit_prover_for_keygen: bool) {
        println!("negative test ...");
        assert!(!test(a, b, c, use_circuit_prover_for_keygen));
        println!("OK");
    }

    fn positive_test(a: Fp, b: Fp, c: Fp, use_circuit_prover_for_keygen: bool) {
        println!("positive test ...");
        assert!(test(a, b, c, use_circuit_prover_for_keygen));
        println!("OK");
    }

    let a = Fp::from(50);
    let b = Fp::from(27);
    let c = Fp::from(50 ^ 27);
    positive_test(a, b, c, true);
    //positive_test(a, b, c, false);
    negative_test(a, b + Fp::one(), c, true);

    let mut rng = OsRng;
    let a = Fp::from(rng.gen_range(0..u32::MAX) as u64);
    let b = Fp::from(rng.gen_range(0..u32::MAX) as u64);
    let c = fp_xor(a, b);
    positive_test(a, b, c, true);
    //positive_test(a, b, c, false);
    negative_test(a, b + Fp::one(), c, true);

    let a = Fp::from(u64::MAX); // not a valid 32-bit word
    let b = Fp::from(u32::MAX as u64);
    let c = fp_xor(a, b);
    negative_test(a, b, c, true);
    negative_test(a, b, c, false);

    let a = Fp::from(u32::MAX as u64);
    let b = Fp::from(u64::MAX); // not a valid 32-bit word
    let c = fp_xor(a, b);
    negative_test(a, b, c, true);
    negative_test(a, b, c, false);
}

fn fp_xor(a: Fp, b: Fp) -> Fp {
    let xor = a
        .to_repr()
        .iter()
        .zip(b.to_repr().iter())
        .map(|(byte1, byte2)| *byte1 ^ *byte2)
        .collect::<Vec<u8>>();
    Fp::from_repr(xor.try_into().unwrap()).unwrap()
}

#[test]
fn test_u32words_xor_mocked_prover() {
    let a = Value::known(Fp::from(50));
    let b = Value::known(Fp::from(27));
    let c = Fp::from(50 ^ 27);

    let circuit = U32WordXorCircuit { a, b };

    let prover =
        MockProver::run(circuit.k(), &circuit, vec![vec![c]]).expect("couldn't run mocked prover");
    assert!(prover.verify().is_ok());
}

#[test]
fn test_pack_chip() {
    struct TestCircuit {
        a: Value<Fp>,
        b: Value<Fp>,
    }

    impl TestCircuit {
        fn k(&self) -> u32 {
            7
        }
    }

    impl Circuit<Fp> for TestCircuit {
        type Config = (RangeCheckConfig, AssignFp32BitsConfig, PackChipConfig);
        type FloorPlanner = SimpleFloorPlanner;

        fn without_witnesses(&self) -> Self {
            TestCircuit {
                a: Value::unknown(),
                b: Value::unknown(),
            }
        }

        fn configure(meta: &mut ConstraintSystem<Fp>) -> Self::Config {
            let range_check_config = RangeCheckChip::configure(meta);
            let assign_32bits_config = AssignFp32BitsChip::configure(meta);
            let pack_chip_config = PackChip::configure(meta);

            (range_check_config, assign_32bits_config, pack_chip_config)
        }

        fn synthesize(
            &self,
            config: Self::Config,
            mut layouter: impl Layouter<Fp>,
        ) -> Result<(), Error> {
            let range_check_chip = RangeCheckChip::construct(config.0);
            let assign_32bits_chip = AssignFp32BitsChip::construct(config.1);
            let pack_chip = PackChip::construct(config.2);

            let assigned_fp_a =
                range_check_chip.range_check(layouter.namespace(|| "range check of a"), self.a)?;

            let assigned_fp_a_bits = assign_32bits_chip
                .assign_32bits_of_fp(layouter.namespace(|| "assign 32 bits of a"), self.a)?;

            pack_chip.check_packing(
                layouter.namespace(|| "pack a"),
                assigned_fp_a,
                assigned_fp_a_bits,
            )?;

            let assigned_fp_b =
                range_check_chip.range_check(layouter.namespace(|| "range check of b"), self.b)?;

            let assigned_fp_b_bits = assign_32bits_chip
                .assign_32bits_of_fp(layouter.namespace(|| "assign 32 bits of b"), self.b)?;

            pack_chip.check_packing(
                layouter.namespace(|| "pack b"),
                assigned_fp_b,
                assigned_fp_b_bits,
            )?;

            Ok(())
        }
    }

    let a = Value::known(Fp::from(u32::MAX as u64));
    let b = Value::known(Fp::from(u32::MAX as u64));
    let circuit = TestCircuit { a, b };

    let prover =
        MockProver::run(circuit.k(), &circuit, vec![]).expect("can't create mocked prover");
    assert!(prover.verify().is_ok());
}

#[test]
fn test_assign32bits_chip() {
    struct TestCircuit {
        a: Value<Fp>,
    }

    impl TestCircuit {
        fn k(&self) -> u32 {
            6
        }
    }

    impl Circuit<Fp> for TestCircuit {
        type Config = AssignFp32BitsConfig;
        type FloorPlanner = SimpleFloorPlanner;

        fn without_witnesses(&self) -> Self {
            TestCircuit {
                a: Value::unknown(),
            }
        }

        fn configure(meta: &mut ConstraintSystem<Fp>) -> Self::Config {
            AssignFp32BitsChip::configure(meta)
        }

        fn synthesize(
            &self,
            config: Self::Config,
            layouter: impl Layouter<Fp>,
        ) -> Result<(), Error> {
            let chip = AssignFp32BitsChip::construct(config);
            chip.assign_32bits_of_fp(layouter, self.a)?;
            Ok(())
        }
    }

    let a = Value::known(Fp::from(u32::MAX as u64));

    let circuit = TestCircuit { a };

    let prover =
        MockProver::run(circuit.k(), &circuit, vec![]).expect("couldn't run mocked prover");
    assert!(prover.verify().is_ok());
}

#[test]
fn test_range_check() {
    struct TestCircuit {
        a: Value<Fp>,
    }

    impl TestCircuit {
        fn k(&self) -> u32 {
            5
        }
    }
    impl Circuit<Fp> for TestCircuit {
        type Config = RangeCheckConfig;
        type FloorPlanner = SimpleFloorPlanner;

        fn without_witnesses(&self) -> Self {
            TestCircuit {
                a: Value::unknown(),
            }
        }

        fn configure(meta: &mut ConstraintSystem<Fp>) -> Self::Config {
            RangeCheckChip::configure(meta)
        }

        fn synthesize(
            &self,
            config: Self::Config,
            layouter: impl Layouter<Fp>,
        ) -> Result<(), Error> {
            let chip = RangeCheckChip::construct(config);
            chip.range_check(layouter, self.a)?;
            Ok(())
        }
    }

    let a = Value::known(Fp::from(u32::MAX as u64));

    let circuit = TestCircuit { a };

    let prover = MockProver::run(circuit.k(), &circuit, vec![]).expect("can't verify proof");
    assert!(prover.verify().is_ok());
}
