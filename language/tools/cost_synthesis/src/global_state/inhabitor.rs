// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

//! Random valid type inhabitant generation.
use crate::common::*;
use rand::{rngs::StdRng, Rng, SeedableRng};
use std::collections::HashMap;
use types::{account_address::AccountAddress, byte_array::ByteArray, language_storage::ModuleId};
use vm::{
    access::*,
    file_format::{
        MemberCount, ModuleHandle, SignatureToken, StructDefinition, StructDefinitionIndex,
        StructFieldInformation, StructHandleIndex, TableIndex,
    },
};
use vm_runtime::{
    code_cache::module_cache::ModuleCache, loaded_data::loaded_module::LoadedModule, value::*,
};

/// A wrapper around state that is used to generate random valid inhabitants for types.
pub struct RandomInhabitor<'alloc, 'txn>
where
    'alloc: 'txn,
{
    /// The source of pseudo-randomness.
    gen: StdRng,

    /// Certain instructions require indices into the various tables within the module.
    /// We store a reference to the loaded module context that we are currently in so that we can
    /// generate valid references into these tables. When generating a module universe this is the
    /// root module that has pointers to all other modules.
    root_module: &'txn LoadedModule,

    /// The module cache for all of the other modules in the universe. We need this in order to
    /// resolve struct and function handles to other modules other then the root module.
    module_cache: &'txn dyn ModuleCache<'alloc>,

    /// A reverse lookup table to find the struct definition for a struct handle. Needed for
    /// generating an inhabitant for a struct SignatureToken. This is lazily populated.
    struct_handle_table: HashMap<ModuleId, HashMap<String, StructDefinitionIndex>>,
}

impl<'alloc, 'txn> RandomInhabitor<'alloc, 'txn>
where
    'alloc: 'txn,
{
    /// Create a new random type inhabitor.
    ///
    /// It initializes each of the internal resolution tables for structs and function handles to
    /// be empty.
    pub fn new(
        root_module: &'txn LoadedModule,
        module_cache: &'txn dyn ModuleCache<'alloc>,
    ) -> Self {
        let seed: [u8; 32] = [0; 32];
        Self {
            gen: StdRng::from_seed(seed),
            root_module,
            module_cache,
            struct_handle_table: HashMap::new(),
        }
    }

    fn to_module_id(&self, module_handle: &ModuleHandle) -> ModuleId {
        let address = *self.root_module.address_at(module_handle.address);
        let name = self.root_module.string_at(module_handle.name);
        ModuleId::new(address, name.to_string())
    }

    fn next_int(&mut self) -> u64 {
        u64::from(self.gen.gen_range(0, u32::max_value()))
    }

    fn next_bool(&mut self) -> bool {
        // Flip a coin
        self.gen.gen_bool(0.5)
    }

    fn next_bytearray(&mut self) -> ByteArray {
        let len: usize = self.gen.gen_range(1, BYTE_ARRAY_MAX_SIZE);
        let bytes: Vec<u8> = (0..len).map(|_| self.gen.gen::<u8>()).collect();
        ByteArray::new(bytes)
    }

    fn next_str(&mut self) -> String {
        let len: usize = self.gen.gen_range(1, MAX_STRING_SIZE);
        (0..len).map(|_| self.gen.gen::<char>()).collect::<String>()
    }

    fn next_addr(&mut self) -> AccountAddress {
        AccountAddress::new(self.gen.gen())
    }

    fn resolve_struct_handle(
        &mut self,
        struct_handle_index: StructHandleIndex,
    ) -> (
        &'txn LoadedModule,
        &'txn StructDefinition,
        StructDefinitionIndex,
    ) {
        let struct_handle = self.root_module.struct_handle_at(struct_handle_index);
        let struct_name = self.root_module.string_at(struct_handle.name);
        let module_handle = self.root_module.module_handle_at(struct_handle.module);
        let module_id = self.to_module_id(module_handle);
        let module = self
            .module_cache
            .get_loaded_module(&module_id)
            .expect("[Module Lookup] Invariant violation while looking up module")
            .expect("[Module Lookup] Runtime error while looking up module")
            .expect("[Module Lookup] Unable to find module");
        let struct_def_idx = if self.struct_handle_table.contains_key(&module_id) {
            self.struct_handle_table
                .get(&module_id)
                .expect("[Struct Definition Lookup] Unable to get struct handles for module")
                .get(struct_name)
        } else {
            let entry = self
                .struct_handle_table
                .entry(module_id)
                .or_insert_with(|| {
                    module
                        .struct_defs()
                        .iter()
                        .enumerate()
                        .map(|(struct_def_index, struct_def)| {
                            let handle = module.struct_handle_at(struct_def.struct_handle);
                            let name = module.string_at(handle.name).to_string();
                            (
                                name,
                                StructDefinitionIndex::new(struct_def_index as TableIndex),
                            )
                        })
                        .collect()
                });
            entry.get(struct_name)
        }
        .expect("[Struct Definition Lookup] Unable to get struct definition for struct handle");
        let struct_def = module.struct_def_at(*struct_def_idx);
        (module, struct_def, *struct_def_idx)
    }

    /// Build an inhabitant of the type given by `sig_token`. Note that as opposed to the
    /// inhabitant generation that is performed in the `StackGenerator` this does _not_ take the
    /// instruction and generates inhabitants in a semantically agnostic way.
    pub fn inhabit(&mut self, sig_token: &SignatureToken) -> Local {
        match sig_token {
            SignatureToken::Bool => Local::bool(self.next_bool()),
            SignatureToken::U64 => Local::u64(self.next_int()),
            SignatureToken::String => Local::string(self.next_str()),
            SignatureToken::Address => Local::address(self.next_addr()),
            SignatureToken::Reference(sig) | SignatureToken::MutableReference(sig) => {
                let underlying_value = self.inhabit(&*sig);
                underlying_value
                    .borrow_local()
                    .expect("Unable to generate valid reference value")
            }
            SignatureToken::ByteArray => Local::bytearray(self.next_bytearray()),
            SignatureToken::Struct(struct_handle_idx, _) => {
                assert!(self.root_module.struct_defs().len() > 1);
                let struct_definition = self
                    .root_module
                    .struct_def_at(self.resolve_struct_handle(*struct_handle_idx).2);
                let (num_fields, index) = match struct_definition.field_information {
                    StructFieldInformation::Native => {
                        panic!("[Struct Generation] Unexpected native struct")
                    }
                    StructFieldInformation::Declared {
                        field_count,
                        fields,
                    } => (field_count as usize, fields),
                };
                let fields = self
                    .root_module
                    .field_def_range(num_fields as MemberCount, index);
                let mutvals = fields
                    .iter()
                    .map(|field| {
                        self.inhabit(
                            &self.root_module
                                .type_signature_at(field.signature)
                                .0
                        )
                        .value()
                        .expect("[Struct Generation] Unable to get underlying value for generated struct field.")
                    })
                    .collect();
                Local::struct_(mutvals)
            }
            SignatureToken::TypeParameter(_) => unimplemented!(),
        }
    }
}
