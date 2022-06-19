#![doc = include_str!("../README.md")]
#![warn(missing_docs)]
#![warn(unused_extern_crates)]
#![forbid(unsafe_code)]
#![forbid(where_clauses_object_safety)]

use huff_utils::{
    abi::*,
    artifact::*,
    ast::*,
    bytecode::*,
    error::CodegenError,
    evm::Opcode,
    prelude::{
        bytes32_to_string, format_even_bytes, pad_n_bytes, CodegenErrorKind, FileSource, Span,
    },
    types::EToken,
};
use std::{collections::HashMap, fs, path::Path, str::FromStr};

/// ### Codegen
///
/// Code Generation Manager responsible for generating bytecode from a [Contract]() Abstract Syntax
/// Tree.
///
/// #### Usage
///
/// The canonical way to instantiate a Codegen instance is using the public associated
/// [new](Codegen::new) function.
///
///
/// ```rust
/// use huff_codegen::Codegen;
/// let cg = Codegen::new();
/// ```
#[derive(Debug, Default, PartialEq, Eq, Clone)]
pub struct Codegen {
    /// The Input AST
    pub ast: Option<Contract>,
    /// A cached codegen output artifact
    pub artifact: Option<Artifact>,
    /// Intermediate main bytecode store
    pub main_bytecode: Option<String>,
    /// Intermediate constructor bytecode store
    pub constructor_bytecode: Option<String>,
}

impl Codegen {
    /// Public associated function to instantiate a new Codegen instance.
    pub fn new() -> Self {
        Self { ast: None, artifact: None, main_bytecode: None, constructor_bytecode: None }
    }

    /// Helper function to find a macro or generate a CodegenError
    pub(crate) fn get_macro_by_name(
        name: &str,
        contract: &Contract,
    ) -> Result<MacroDefinition, CodegenError> {
        if let Some(m) = contract.find_macro_by_name(name) {
            Ok(m)
        } else {
            tracing::error!(target: "codegen", "MISSING \"{}\" MACRO!", name);
            Err(CodegenError {
                kind: CodegenErrorKind::MissingMacroDefinition(name.to_string()),
                span: AstSpan(vec![Span { start: 0, end: 0, file: None }]),
                token: None,
            })
        }
    }

    /// Generates main bytecode from a Contract AST
    pub fn generate_main_bytecode(contract: &Contract) -> Result<String, CodegenError> {
        // Find the main macro
        let m_macro = Codegen::get_macro_by_name("MAIN", contract)?;

        // For each MacroInvocation Statement, recurse into bytecode
        let bytecode_res: BytecodeRes = Codegen::macro_to_bytecode(
            m_macro.clone(),
            contract,
            &mut vec![m_macro],
            0,
            &mut Vec::default(),
        )?;

        // Generate the fully baked bytecode
        Codegen::gen_table_bytecode(bytecode_res, contract)
    }

    /// Generates constructor bytecode from a Contract AST
    pub fn generate_constructor_bytecode(contract: &Contract) -> Result<String, CodegenError> {
        // Find the constructor macro
        let c_macro = Codegen::get_macro_by_name("CONSTRUCTOR", contract)?;

        // For each MacroInvocation Statement, recurse into bytecode
        let bytecode_res: BytecodeRes = Codegen::macro_to_bytecode(
            c_macro.clone(),
            contract,
            &mut vec![c_macro],
            0,
            &mut Vec::default(),
        )?;

        // Generate the bytecode return string
        let bytecode = bytecode_res.bytes.iter().map(|(_, b)| b.0.to_string()).collect();
        Ok(bytecode)
    }

    /// Adds table bytecode at the end of the `recurse_bytecode`
    /// output and fills table JUMPDEST placeholders
    pub fn gen_table_bytecode(
        res: BytecodeRes,
        contract: &Contract,
    ) -> Result<String, CodegenError> {
        if !res.unmatched_jumps.is_empty() {
            tracing::error!(
                target: "codegen",
                "Source contains unmatched jump labels \"{}\"",
                res.unmatched_jumps.iter().map(|uj| uj.label.to_string()).collect::<Vec<String>>().join(", ")
            );
            return Err(CodegenError {
                kind: CodegenErrorKind::UnmatchedJumpLabel,
                span: AstSpan(vec![]),
                token: None,
            })
        }

        tracing::info!(target: "codegen", "GENERATING JUMPTABLE BYTECODE");

        let mut bytecode = res.bytes.into_iter().map(|(_, b)| b.0).collect::<String>();
        let mut table_offsets: HashMap<String, usize> = HashMap::new(); // table name -> bytecode offset
        let mut table_offset = bytecode.len() / 2;

        contract.tables.iter().for_each(|jt| {
            table_offsets.insert(jt.name.to_string(), table_offset);
            let size = bytes32_to_string(&jt.size, false).parse::<usize>().unwrap(); // TODO: Error handling
            table_offset += size;

            tracing::info!(target: "codegen", "GENERATING BYTECODE FOR TABLE: \"{}\"", jt.name);

            let table_code = jt
                .statements
                .iter()
                .map(|s| {
                    if let StatementType::LabelCall(label) = &s.ty {
                        let offset = res.label_indices.get(label).unwrap(); // TODO: Error handling
                        let hex = format_even_bytes(format!("{:02x}", offset));

                        pad_n_bytes(
                            hex.as_str(),
                            if matches!(jt.kind, TableKind::JumpTablePacked) { 0x02 } else { 0x20 },
                        )
                    } else {
                        String::default()
                    }
                })
                .collect::<String>();
            tracing::info!(target: "codegen", "SUCCESSFULLY GENERATED BYTECODE FOR TABLE: \"{}\"", jt.name);
            bytecode = format!("{}{}", bytecode, table_code);
        });

        res.table_instances.iter().for_each(|jump| {
            if let Some(o) = table_offsets.get(&jump.label) {
                let before = &bytecode[0..jump.bytecode_index * 2 + 2];
                let after = &bytecode[jump.bytecode_index * 2 + 6..];

                bytecode =
                    format!("{}{}{}", before, pad_n_bytes(format!("{:02x}", o).as_str(), 2), after);
                tracing::info!(target: "codegen", "FILLED JUMPDEST FOR LABEL \"{}\"", jump.label);
            } else {
                tracing::error!(
                    target: "codegen",
                    "Jump table offset not present for jump label \"{}\"",
                    jump.label
                );
            }
        });

        Ok(bytecode)
    }

    /// Recurses a MacroDefinition to generate Bytecode
    /// TODO: Separate all bytecode generation into separate functions
    ///
    /// # Arguments
    ///
    /// * `macro_def` - Macro definition to convert to bytecode
    /// * `contract` - Reference to the `Contract` AST generated by the parser
    /// * `scope` - Current scope of the recursion. Contains all macro definitions recursed so far.
    /// * `offset` - Current bytecode offset
    /// * `mis` - Vector of tuples containing parent macro invocations as well as their offsets.
    pub fn macro_to_bytecode(
        macro_def: MacroDefinition,
        contract: &Contract,
        scope: &mut Vec<MacroDefinition>,
        mut offset: usize,
        mis: &mut Vec<(usize, MacroInvocation)>,
    ) -> Result<BytecodeRes, CodegenError> {
        // Get intermediate bytecode representation of the AST
        let mut bytes: Vec<(usize, Bytes)> = Vec::default();
        let ir_bytes = macro_def.to_irbytecode()?.0;

        // Define outer loop variables
        let mut jump_table = JumpTable::new();
        let mut label_indices = LabelIndices::new();
        let mut table_instances = Jumps::new();

        // Loop through all intermediate bytecode representations generated from the AST
        for (ir_bytes_index, ir_byte) in ir_bytes.into_iter().enumerate() {
            let starting_offset = offset;
            match ir_byte.ty {
                IRByteType::Bytes(b) => {
                    offset += b.0.len() / 2;
                    bytes.push((starting_offset, b));
                }
                IRByteType::Constant(name) => {
                    // Get the first `ConstantDefinition` that matches the constant's name
                    let constant = if let Some(m) =
                        contract.constants.iter().find(|const_def| const_def.name.eq(&name))
                    {
                        m
                    } else {
                        tracing::error!(target: "codegen", "MISSING CONSTANT DEFINITION \"{}\"", name);

                        return Err(CodegenError {
                            kind: CodegenErrorKind::MissingConstantDefinition(name),
                            span: ir_byte.span,
                            token: None,
                        })
                    };

                    // Generate bytecode for the constant
                    // Should always be a `Literal` if storage pointers were derived in the AST
                    // prior to generating the IR bytes.
                    tracing::info!(target: "codegen", "FOUND CONSTANT DEFINITION: {}", constant.name);
                    let push_bytes = match &constant.value {
                        ConstVal::Literal(l) => {
                            let hex_literal: String = bytes32_to_string(l, false);
                            format!("{:02x}{}", 95 + hex_literal.len() / 2, hex_literal)
                        }
                        ConstVal::FreeStoragePointer(fsp) => {
                            // If this is reached in codegen stage, the `derive_storage_pointers`
                            // method was not called on the AST.
                            tracing::error!(target: "codegen", "STORAGE POINTERS INCORRECTLY DERIVED FOR \"{:?}\"", fsp);
                            return Err(CodegenError {
                                kind: CodegenErrorKind::StoragePointersNotDerived,
                                span: constant.span.clone(),
                                token: None,
                            })
                        }
                    };

                    // Add the constant's bytecode to the final result
                    offset += push_bytes.len() / 2;
                    tracing::info!(target: "codegen", "OFFSET: {}, PUSH BYTES: {:?}", offset, push_bytes);
                    bytes.push((starting_offset, Bytes(push_bytes)));
                }
                IRByteType::Statement(s) => {
                    tracing::debug!(target: "codegen", "Got Statement: {:?}", s);
                    match s.ty {
                        StatementType::MacroInvocation(mi) => {
                            // Get the macro definition that matches the name of this invocation
                            let ir_macro =
                                if let Some(m) = contract.find_macro_by_name(&mi.macro_name) {
                                    m
                                } else {
                                    tracing::error!(
                                        target: "codegen",
                                        "MISSING MACRO INVOCATION \"{}\"",
                                        mi.macro_name
                                    );
                                    return Err(CodegenError {
                                        kind: CodegenErrorKind::MissingMacroDefinition(
                                            mi.macro_name.clone(),
                                        ),
                                        span: AstSpan(vec![Span { start: 0, end: 0, file: None }]),
                                        token: None,
                                    })
                                };

                            tracing::info!(target: "codegen", "FOUND INNER MACRO: {}", ir_macro.name);

                            // Recurse into macro invocation
                            scope.push(ir_macro.clone());
                            mis.push((offset, mi.clone()));

                            let mut res: BytecodeRes = match Codegen::macro_to_bytecode(
                                ir_macro.clone(),
                                contract,
                                scope,
                                offset,
                                mis,
                            ) {
                                Ok(r) => r,
                                Err(e) => {
                                    tracing::error!(
                                        target: "codegen",
                                        "FAILED TO RECURSE INTO MACRO \"{}\"",
                                        ir_macro.name
                                    );
                                    return Err(e)
                                }
                            };

                            // Set jump table values
                            tracing::debug!(target: "codegen", "Setting Unmatched Jumps to new index: {}", ir_bytes_index);
                            tracing::debug!(target: "codegen", "Unmatched jumps: {:?}", res.unmatched_jumps);
                            for j in res.unmatched_jumps.iter_mut() {
                                let new_index = j.bytecode_index;
                                j.bytecode_index = 0;
                                let mut new_jumps = if let Some(jumps) = jump_table.get(&new_index)
                                {
                                    jumps.clone()
                                } else {
                                    vec![]
                                };
                                new_jumps.push(j.clone());
                                jump_table.insert(new_index, new_jumps);
                            }
                            table_instances.extend(res.table_instances);
                            label_indices.extend(res.label_indices);

                            // Increase offset by byte length of recursed macro
                            offset += res.bytes.iter().map(|(_, b)| b.0.len()).sum::<usize>() / 2;
                            // Add the macro's bytecode to the final result
                            bytes = [bytes, res.bytes].concat()
                        }
                        StatementType::Label(label) => {
                            // Add JUMPDEST opcode to final result and add to label_indices
                            tracing::info!(target: "codegen", "RECURSE BYTECODE GOT LABEL: {:?}", label);
                            label_indices.insert(label.name, offset);
                            bytes.push((offset, Bytes(Opcode::Jumpdest.to_string())));
                            offset += 1;
                        }
                        StatementType::LabelCall(label) => {
                            // Generate code for a `LabelCall`
                            // PUSH2 + 2 byte destination (placeholder for now, filled at the bottom
                            // of this function)
                            tracing::info!(target: "codegen", "RECURSE BYTECODE GOT LABEL CALL: {}", label);
                            jump_table.insert(
                                offset,
                                vec![Jump { label, bytecode_index: 0 }], /* Insert label with a
                                                                          * placeholder bytecode
                                                                          * index */
                            );
                            bytes.push((offset, Bytes(format!("{}xxxx", Opcode::Push2))));
                            offset += 3;
                        }
                        StatementType::BuiltinFunctionCall(bf) => {
                            // Generate code for a `BuiltinFunctionCall`
                            // __codesize, __tablesize, or __tablestart
                            // TODO: Inline docs
                            tracing::info!(target: "codegen", "RECURSE BYTECODE GOT BUILTIN FUNCTION CALL: {:?}", bf);
                            match bf.kind {
                                BuiltinFunctionKind::Codesize => {
                                    let ir_macro = if let Some(m) = contract
                                        .find_macro_by_name(bf.args[0].name.as_ref().unwrap())
                                    {
                                        m
                                    } else {
                                        tracing::error!(
                                            target: "codegen",
                                            "MISSING MACRO PASSED TO __codesize \"{}\"",
                                            bf.args[0].name.as_ref().unwrap()
                                        );
                                        return Err(CodegenError {
                                            kind: CodegenErrorKind::MissingMacroDefinition(
                                                bf.args[0].name.as_ref().unwrap().to_string(), /* yuck */
                                            ),
                                            span: AstSpan(vec![Span {
                                                start: 0,
                                                end: 0,
                                                file: None,
                                            }]),
                                            token: None,
                                        })
                                    };

                                    let res: BytecodeRes = match Codegen::macro_to_bytecode(
                                        ir_macro.clone(),
                                        contract,
                                        scope,
                                        offset,
                                        mis,
                                    ) {
                                        Ok(r) => r,
                                        Err(e) => {
                                            tracing::error!(
                                                target: "codegen",
                                                "FAILED TO RECURSE INTO MACRO \"{}\"",
                                                ir_macro.name
                                            );
                                            return Err(e)
                                        }
                                    };

                                    let size = format_even_bytes(format!(
                                        "{:02x}",
                                        (res.bytes.iter().map(|(_, b)| b.0.len()).sum::<usize>() /
                                            2)
                                    ));
                                    let push_bytes = format!("{:02x}{}", 95 + size.len() / 2, size);

                                    offset += push_bytes.len() / 2;
                                    bytes.push((starting_offset, Bytes(push_bytes)));
                                }
                                BuiltinFunctionKind::Tablesize => {
                                    let ir_table = if let Some(t) = contract
                                        .find_table_by_name(bf.args[0].name.as_ref().unwrap())
                                    {
                                        t
                                    } else {
                                        tracing::error!(
                                            target: "codegen",
                                            "MISSING TABLE PASSED TO __tablesize \"{}\"",
                                            bf.args[0].name.as_ref().unwrap()
                                        );
                                        return Err(CodegenError {
                                            kind: CodegenErrorKind::MissingMacroDefinition(
                                                bf.args[0].name.as_ref().unwrap().to_string(), /* yuck */
                                            ),
                                            span: AstSpan(vec![Span {
                                                start: 0,
                                                end: 0,
                                                file: None,
                                            }]),
                                            token: None,
                                        })
                                    };

                                    let size = bytes32_to_string(&ir_table.size, false);
                                    let push_bytes = format!("{:02x}{}", 95 + size.len() / 2, size);

                                    offset += push_bytes.len() / 2;
                                    bytes.push((starting_offset, Bytes(push_bytes)));
                                }
                                BuiltinFunctionKind::Tablestart => {
                                    table_instances.push(Jump {
                                        label: bf.args[0].name.as_ref().unwrap().to_owned(),
                                        bytecode_index: offset,
                                    });

                                    bytes.push((offset, Bytes(format!("{}xxxx", Opcode::Push2))));
                                    offset += 3;
                                }
                            }
                        }
                        sty => {
                            tracing::error!(target: "codegen", "CURRENT MACRO DEF: {}", macro_def.name);
                            tracing::error!(target: "codegen", "UNEXPECTED STATEMENT: {:?}", sty);
                            return Err(CodegenError {
                                kind: CodegenErrorKind::InvalidMacroStatement,
                                span: s.span,
                                token: None,
                            })
                        }
                    }
                }
                IRByteType::ArgCall(arg_name) => {
                    // Bubble up arg call by looking through the previous scopes. Once the arg
                    // value is found, add it to `bytes`
                    if let Err(e) = Codegen::bubble_arg_call(
                        &arg_name,
                        &mut bytes,
                        &macro_def,
                        contract,
                        scope,
                        &mut offset,
                        mis,
                        &mut jump_table,
                    ) {
                        return Err(e)
                    }

                    tracing::debug!(target: "codegen", "^^ BUBBLING FINISHED ^^ LEFT OVER MACRO INVOCATIONS: {}", mis.len());
                    tracing::debug!(target: "codegen", "^^ BUBBLING FINISHED ^^ CURRENT MACRO DEF: {}", macro_def.name);
                }
            }
        }

        // We're done, let's pop off the macro invocation
        if mis.pop().is_none() {
            tracing::warn!(target: "codegen", "ATTEMPTED MACRO INVOCATION POP FAILED AT SCOPE: {}", scope.len());
        }

        let bytecode: String = bytes.iter().map(|byte| byte.0.to_string()).collect();
        tracing::info!(target: "codegen", "MACRO \"{}\" GENERATED BYTECODE EXCLUDING JUMPS: {}", macro_def.name, bytecode);

        // Fill JUMPDEST placeholders
        let mut unmatched_jumps = Jumps::default();
        let bytes =
            bytes.into_iter().fold(Vec::default(), |mut acc, (code_index, mut formatted_bytes)| {
                tracing::debug!(target: "codegen", "Formatted bytes: {:#?}", &formatted_bytes);

                // Check if a jump table exists at `code_index` (starting offset of `b`)
                if let Some(jt) = jump_table.get(&code_index) {
                    // Loop through jumps inside of the found JumpTable
                    for jump in jt {
                        tracing::debug!(target: "codegen", "Getting Jump For Index: {}", code_index);
                        tracing::debug!(target: "codegen", "Found Jump: {:?}", jump);
                        tracing::debug!(target: "codegen", "Filling Label Call: {}", jump.label);

                        // Check if the jump label has been defined. If not, add `jump` to the unmatched
                        // jumps and define its `bytecode_index` at `code_index`
                        if let Some(jump_index) = label_indices.get(jump.label.as_str()) {
                            // Format the jump index as a 2 byte hex number
                            let jump_value = format!("{:04x}", jump_index);
                            tracing::debug!(target: "codegen", "Got Jump Value: {}", jump_value);
                            tracing::debug!(target: "codegen", "Jump Bytecode index: {}", jump.bytecode_index);

                            // Get the bytes before & after the placeholder
                            let before = &formatted_bytes.0[0..jump.bytecode_index + 2];
                            let after = &formatted_bytes.0[jump.bytecode_index + 6..];

                            // Check if a jump dest placeholder is present
                            if !&formatted_bytes.0[jump.bytecode_index + 2..jump.bytecode_index + 6].eq("xxxx") {
                                tracing::error!(
                                    target: "codegen",
                                    "JUMP DESTINATION PLACEHOLDER NOT FOUND FOR JUMPLABEL {}",
                                    jump.label
                                );
                            }

                            // Replace the "xxxx" placeholder with the jump value
                            formatted_bytes = Bytes(format!("{}{}{}", before, jump_value, after));
                        } else {
                            tracing::debug!(target: "codegen", "Inserting unmatched jump: {:?}", jump);

                            // The jump did not have a corresponding label index. Add it to the
                            // unmatched jumps vec.
                            unmatched_jumps.push(Jump {
                                label: jump.label.clone(),
                                bytecode_index: code_index
                            });
                        }
                    }
                }

                acc.push((code_index, formatted_bytes));
                acc
            });

        Ok(BytecodeRes { bytes, label_indices, unmatched_jumps, table_instances })
    }

    /// Arg Call Bubbling
    #[allow(clippy::too_many_arguments)]
    pub fn bubble_arg_call(
        arg_name: &str,
        bytes: &mut Vec<(usize, Bytes)>,
        macro_def: &MacroDefinition,
        contract: &Contract,
        scope: &mut Vec<MacroDefinition>,
        offset: &mut usize,
        // mis: Parent macro invocations and their indices
        mis: &mut Vec<(usize, MacroInvocation)>,
        jump_table: &mut JumpTable,
    ) -> Result<(), CodegenError> {
        // Args can be literals, labels, opcodes, or constants
        // !! IF THERE IS AMBIGUOUS NOMENCLATURE
        // !! (E.G. BOTH OPCODE AND LABEL ARE THE SAME STRING)
        // !! COMPILATION _WILL_ ERROR

        tracing::warn!(target: "codegen", "**BUBBLING** \"{}\"", macro_def.name);

        let starting_offset = *offset;
        // Check Constant Definitions
        if let Some(constant) =
            contract.constants.iter().find(|const_def| const_def.name.eq(arg_name))
        {
            tracing::info!(target: "codegen", "ARGCALL IS CONSTANT: {:?}", constant);
            let push_bytes = match &constant.value {
                ConstVal::Literal(l) => {
                    let hex_literal: String = bytes32_to_string(l, false);
                    format!("{:02x}{}", 95 + hex_literal.len() / 2, hex_literal)
                }
                ConstVal::FreeStoragePointer(fsp) => {
                    // If this is reached in codegen stage, the
                    // `derive_storage_pointers`
                    // method was not called on the AST.
                    tracing::error!(target: "codegen", "STORAGE POINTERS INCORRECTLY DERIVED FOR \"{:?}\"", fsp);
                    return Err(CodegenError {
                        kind: CodegenErrorKind::StoragePointersNotDerived,
                        span: AstSpan(vec![]),
                        token: None,
                    })
                }
            };
            *offset += push_bytes.len() / 2;
            tracing::info!(target: "codegen", "OFFSET: {}, PUSH BYTES: {:?}", offset, push_bytes);
            bytes.push((starting_offset, Bytes(push_bytes)));
        } else if let Ok(o) = Opcode::from_str(arg_name) {
            // Check Opcode Definition
            let b = Bytes(o.to_string());
            *offset += b.0.len() / 2;
            tracing::info!(target: "codegen", "RECURSE_BYTECODE ARG CALL FOUND OPCODE: {:?}", b);
            bytes.push((starting_offset, b));
        } else if let Some(macro_invoc) = mis.last() {
            // Literal & Arg Call Check
            // First get this arg_nam position in the macro definition params
            if let Some(pos) = macro_def
                .parameters
                .iter()
                .position(|r| r.name.as_ref().map_or(false, |s| s.eq(arg_name)))
            {
                tracing::info!(target: "codegen", "GOT \"{}\" POS IN ARG LIST: {}", arg_name, pos);

                if let Some(arg) = macro_invoc.1.args.get(pos) {
                    tracing::info!(target: "codegen", "GOT \"{:?}\" ARG FROM MACRO INVOCATION", arg);
                    match arg {
                        MacroArg::Literal(l) => {
                            tracing::info!(target: "codegen", "GOT LITERAL {} ARG FROM MACRO INVOCATION", bytes32_to_string(l, false));

                            let hex_literal: String = bytes32_to_string(l, false);
                            let push_bytes =
                                format!("{:02x}{}", 95 + hex_literal.len() / 2, hex_literal);
                            let b = Bytes(push_bytes);
                            *offset += b.0.len() / 2;
                            bytes.push((starting_offset, b));
                        }
                        MacroArg::ArgCall(ac) => {
                            tracing::info!(target: "codegen", "GOT ARG CALL \"{}\" ARG FROM MACRO INVOCATION", ac);
                            tracing::debug!(target: "codegen", "~~~ BUBBLING UP ARG CALL");
                            let mut new_scope = Vec::from(&scope[..scope.len().saturating_sub(1)]);
                            let bubbled_macro_invocation = new_scope.last().unwrap().clone();
                            tracing::debug!(target: "codegen", "BUBBLING UP WITH MACRO DEF: {}", bubbled_macro_invocation.name);
                            tracing::debug!(target: "codegen", "CURRENT MACRO DEF: {}", macro_def.name);

                            // Only remove an invocation if not at bottom level, otherwise we'll
                            // remove one too many
                            let last_mi = match mis.last() {
                                Some(mi) => mi,
                                None => {
                                    return Err(CodegenError {
                                        kind: CodegenErrorKind::MissingMacroInvocation(
                                            macro_def.name.clone(),
                                        ),
                                        span: AstSpan(vec![]),
                                        token: None,
                                    })
                                }
                            };
                            return if last_mi.1.macro_name.eq(&macro_def.name) {
                                Codegen::bubble_arg_call(
                                    arg_name,
                                    bytes,
                                    &bubbled_macro_invocation,
                                    contract,
                                    &mut new_scope,
                                    offset,
                                    &mut Vec::from(&mis[..mis.len().saturating_sub(1)]),
                                    jump_table,
                                )
                            } else {
                                Codegen::bubble_arg_call(
                                    arg_name,
                                    bytes,
                                    &bubbled_macro_invocation,
                                    contract,
                                    &mut new_scope,
                                    offset,
                                    mis,
                                    jump_table,
                                )
                            }
                        }
                        MacroArg::Ident(iden) => {
                            tracing::debug!(target: "codegen", "FOUND IDENT ARG IN \"{}\" MACRO INVOCATION: \"{}\"!", macro_invoc.1.macro_name, iden);
                            tracing::debug!(target: "codegen", "Macro invocation index: {}", macro_invoc.0);
                            tracing::debug!(target: "codegen", "At offset: {}", *offset);

                            // This should be equivalent to a label call.
                            bytes.push((*offset, Bytes(format!("{}xxxx", Opcode::Push2))));
                            jump_table.insert(
                                *offset,
                                vec![Jump { label: iden.to_owned(), bytecode_index: 0 }],
                            );
                            *offset += 3;
                        }
                    }
                } else {
                    tracing::warn!(target: "codegen", "\"{}\" FOUND IN MACRO DEF BUT NOT IN MACRO INVOCATION!", arg_name);
                }
            } else {
                tracing::warn!(target: "codegen", "\"{}\" NOT IN ARG LIST", arg_name);
            }
        } else {
            // Label can be defined in parent
            // Assume Label Call Otherwise
            tracing::info!(target: "codegen", "RECURSE_BYTECODE ARG CALL DEFAULTING TO LABEL CALL: \"{}\"", arg_name);
            jump_table.insert(
                mis.last().map(|mi| mi.0).unwrap_or_else(|| 0),
                vec![Jump { label: arg_name.to_owned(), bytecode_index: 0 }],
            );
            bytes.push((*offset, Bytes(format!("{}xxxx", Opcode::Push2))));
            *offset += 3;
        }

        Ok(())
    }

    /// Generate a codegen artifact
    ///
    /// # Arguments
    ///
    /// * `args` - A vector of Tokens representing constructor arguments
    /// * `main_bytecode` - The compiled MAIN Macro bytecode
    /// * `constructor_bytecode` - The compiled `CONSTRUCTOR` Macro bytecode
    pub fn churn(
        &mut self,
        file: FileSource,
        args: Vec<ethers::abi::token::Token>,
        main_bytecode: &str,
        constructor_bytecode: &str,
    ) -> Result<Artifact, CodegenError> {
        let mut artifact: &mut Artifact = if let Some(art) = &mut self.artifact {
            art
        } else {
            self.artifact = Some(Artifact::default());
            self.artifact.as_mut().unwrap()
        };

        let contract_length = main_bytecode.len() / 2;
        let constructor_length = constructor_bytecode.len() / 2;

        let contract_size = format!("{:04x}", contract_length);
        let contract_code_offset = format!("{:04x}", 13 + constructor_length);

        let encoded: Vec<Vec<u8>> =
            args.iter().map(|tok| ethers::abi::encode(&[tok.clone()])).collect();
        let hex_args: Vec<String> = encoded.iter().map(|tok| hex::encode(tok.as_slice())).collect();
        let constructor_args = hex_args.join("");

        // Generate the final bytecode
        let bootstrap_code = format!("61{}8061{}6000396000f3", contract_size, contract_code_offset);
        let constructor_code = format!("{}{}", constructor_bytecode, bootstrap_code);
        artifact.bytecode =
            format!("{}{}{}", constructor_code, main_bytecode, constructor_args).to_lowercase();
        artifact.runtime = main_bytecode.to_string().to_lowercase();
        artifact.file = file;
        Ok(artifact.clone())
    }

    /// Encode constructor arguments as ethers::abi::token::Token
    pub fn encode_constructor_args(args: Vec<String>) -> Vec<ethers::abi::token::Token> {
        let tokens: Vec<ethers::abi::token::Token> =
            args.iter().map(|tok| EToken::try_from(tok.clone()).unwrap().0).collect();
        tokens
    }

    /// Export
    ///
    /// Writes a Codegen Artifact out to the specified file.
    ///
    /// # Arguments
    ///
    /// * `out` - Output location to write the serialized json artifact to.
    pub fn export(output: String, art: &Artifact) -> Result<(), CodegenError> {
        let serialized_artifact = serde_json::to_string(art).unwrap();
        // Try to create the parent directory
        let file_path = Path::new(&output);
        if let Some(p) = file_path.parent() {
            if let Err(e) = fs::create_dir_all(p) {
                return Err(CodegenError {
                    kind: CodegenErrorKind::IOError(e.to_string()),
                    span: AstSpan(vec![Span {
                        start: 0,
                        end: 0,
                        file: Some(FileSource {
                            id: uuid::Uuid::new_v4(),
                            path: output,
                            source: None,
                            access: None,
                            dependencies: None,
                        }),
                    }]),
                    token: None,
                })
            }
        }
        if let Err(e) = fs::write(file_path, serialized_artifact) {
            return Err(CodegenError {
                kind: CodegenErrorKind::IOError(e.to_string()),
                span: AstSpan(vec![Span {
                    start: 0,
                    end: 0,
                    file: Some(FileSource {
                        id: uuid::Uuid::new_v4(),
                        path: output,
                        source: None,
                        access: None,
                        dependencies: None,
                    }),
                }]),
                token: None,
            })
        }
        Ok(())
    }

    /// Abi Generation
    ///
    /// Generates an ABI for the given Ast.
    /// Stores the generated ABI in the Codegen `artifact`.
    ///
    /// # Arguments
    ///
    /// * `ast` - The Contract Abstract Syntax Tree
    /// * `output` - An optional output path
    pub fn abi_gen(&mut self, ast: Contract, output: Option<String>) -> Result<Abi, CodegenError> {
        let abi: Abi = ast.into();

        // Set the abi on self
        let art: &Artifact = match &mut self.artifact {
            Some(artifact) => {
                artifact.abi = Some(abi.clone());
                artifact
            }
            None => {
                self.artifact = Some(Artifact { abi: Some(abi.clone()), ..Default::default() });
                self.artifact.as_ref().unwrap()
            }
        };

        // If an output's specified, write the artifact out
        if let Some(o) = output {
            if let Err(e) = Codegen::export(o, art) {
                // Error message is sent to tracing in `export` if an error occurs
                return Err(e)
            }
        }

        // Return the abi
        Ok(abi)
    }
}
