mod component_type_object;

use anyhow::{Ok, Result};
use heck::*;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::mem;
use wit_bindgen_core::abi::{self, AbiVariant, Bindgen, Bitcast, Instruction, LiftLower, WasmType};
use wit_bindgen_core::{
    dealias, uwrite, uwriteln, wit_parser::*, AnonymousTypeGenerator, Direction, Files,
    InterfaceGenerator as _, Ns, WorldGenerator,
};
use wit_component::StringEncoding;

#[derive(Default)]
struct Pascal {
    src: Source,
    opts: Opts,
    h_includes: Vec<String>,
    c_includes: Vec<String>,
    return_pointer_area_size: usize,
    return_pointer_area_align: usize,
    names: Ns,
    needs_string: bool,
    needs_union_int32_float: bool,
    needs_union_float_int32: bool,
    needs_union_int64_double: bool,
    needs_union_double_int64: bool,
    prim_names: HashSet<String>,
    world: String,
    sizes: SizeAlign,
    renamed_interfaces: HashMap<WorldKey, String>,

    world_id: Option<WorldId>,
    dtor_funcs: HashMap<TypeId, String>,
    type_names: HashMap<TypeId, String>,
    resources: HashMap<TypeId, ResourceInfo>,
}

#[derive(Default)]
pub struct ResourceInfo {
    pub direction: Direction,
    own: String,
    borrow: String,
    drop_fn: String,
}

#[derive(Default, Debug, Eq, PartialEq, Clone, Copy)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
pub enum Enabled {
    #[default]
    No,
    Yes,
}

impl std::fmt::Display for Enabled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Yes => write!(f, "yes"),
            Self::No => write!(f, "no"),
        }
    }
}

#[derive(Default, Debug, Clone)]
#[cfg_attr(feature = "clap", derive(clap::Args))]
pub struct Opts {
    /// Skip emitting component allocation helper functions
    #[cfg_attr(feature = "clap", arg(long))]
    pub no_helpers: bool,

    /// Set component string encoding
    #[cfg_attr(feature = "clap", arg(long, default_value_t = StringEncoding::default()))]
    pub string_encoding: StringEncoding,

    /// Skip optional null pointer and boolean result argument signature
    /// flattening
    #[cfg_attr(feature = "clap", arg(long, default_value_t = false))]
    pub no_sig_flattening: bool,

    /// Skip generating an object file which contains type information for the
    /// world that is being generated.
    #[cfg_attr(feature = "clap", arg(long, default_value_t = false))]
    pub no_object_file: bool,

    /// Rename the interface `K` to `V` in the generated source code.
    #[cfg_attr(feature = "clap", arg(long, name = "K=V", value_parser = parse_rename))]
    pub rename: Vec<(String, String)>,

    /// Rename the world in the generated source code and file names.
    #[cfg_attr(feature = "clap", arg(long))]
    pub rename_world: Option<String>,

    /// Add the specified suffix to the name of the custome section containing
    /// the component type.
    #[cfg_attr(feature = "clap", arg(long))]
    pub type_section_suffix: Option<String>,

    /// Configure the autodropping of borrows in exported functions.
    #[cfg_attr(feature = "clap", arg(long, default_value_t = Enabled::default()))]
    pub autodrop_borrows: Enabled,
}

#[cfg(feature = "clap")]
fn parse_rename(name: &str) -> Result<(String, String)> {
    let mut parts = name.splitn(2, '=');
    let to_rename = parts.next().unwrap();
    match parts.next() {
        Some(part) => Ok((to_rename.to_string(), part.to_string())),
        None => anyhow::bail!("`--rename` option must have an `=` in it (e.g. `--rename a=b`)"),
    }
}

impl Opts {
    pub fn build(&self) -> Box<dyn WorldGenerator> {
        let mut r = Pascal::default();
        r.opts = self.clone();
        Box::new(r)
    }
}

#[derive(Debug, Default)]
struct Return {
    scalar: Option<Scalar>,
    retptrs: Vec<Type>,
}

struct CSig {
    name: String,
    sig: String,
    params: Vec<(bool, String)>,
    ret: Return,
    retptrs: Vec<String>,
}

#[derive(Debug)]
enum Scalar {
    Void,
    OptionBool(Type),
    ResultBool(Option<Type>, Option<Type>),
    Type(Type),
}

impl WorldGenerator for Pascal {
    fn preprocess(&mut self, resolve: &Resolve, world: WorldId) {
        self.world = self
            .opts
            .rename_world
            .clone()
            .unwrap_or_else(|| resolve.worlds[world].name.clone());
        self.sizes.fill(resolve);
        self.world_id = Some(world);

        let mut interfaces = HashMap::new();
        let world = &resolve.worlds[world];
        for (key, _item) in world.imports.iter().chain(world.exports.iter()) {
            let name = resolve.name_world_key(key);
            interfaces.insert(name, key.clone());
        }

        for (from, to) in self.opts.rename.iter() {
            match interfaces.get(from) {
                Some(key) => {
                    self.renamed_interfaces.insert(key.clone(), to.clone());
                }
                None => {
                    eprintln!("warning: rename of `{from}` did not match any interfaces");
                }
            }
        }
    }

    fn import_interface(
        &mut self,
        resolve: &Resolve,
        name: &WorldKey,
        id: InterfaceId,
        _files: &mut Files,
    ) -> Result<()> {
        let wasm_import_module = resolve.name_world_key(name);
        let mut gen = self.interface(resolve, true, Some(&wasm_import_module));
        gen.interface = Some((id, name));
        gen.define_interface_types(id);

        for (i, (_name, func)) in resolve.interfaces[id].functions.iter().enumerate() {
            if i == 0 {
                let name = resolve.name_world_key(name);
                uwriteln!(gen.src.h_fns, "\n// Imported Functions from `{name}`");
                uwriteln!(gen.src.c_fns, "\n// Imported Functions from `{name}`");
            }
            gen.import(Some(name), func);
        }

        gen.gen.src.append(&gen.src);

        Ok(())
    }

    fn import_funcs(
        &mut self,
        resolve: &Resolve,
        world: WorldId,
        funcs: &[(&str, &Function)],
        _files: &mut Files,
    ) {
        let name = &resolve.worlds[world].name;
        let mut gen = self.interface(resolve, true, Some("$root"));
        gen.define_function_types(funcs);

        for (i, (_name, func)) in funcs.iter().enumerate() {
            if i == 0 {
                uwriteln!(gen.src.h_fns, "\n// Imported Functions from `{name}`");
                uwriteln!(gen.src.c_fns, "\n// Imported Functions from `{name}`");
            }
            gen.import(None, func);
        }

        gen.gen.src.append(&gen.src);
    }

    fn export_interface(
        &mut self,
        resolve: &Resolve,
        name: &WorldKey,
        id: InterfaceId,
        _files: &mut Files,
    ) -> Result<()> {
        let mut gen = self.interface(resolve, false, None);
        gen.interface = Some((id, name));
        gen.define_interface_types(id);

        for (i, (_name, func)) in resolve.interfaces[id].functions.iter().enumerate() {
            if i == 0 {
                let name = resolve.name_world_key(name);
                uwriteln!(gen.src.h_fns, "\n// Exported Functions from `{name}`");
                uwriteln!(gen.src.c_fns, "\n// Exported Functions from `{name}`");
            }
            gen.export(func, Some(name));
        }

        gen.gen.src.append(&gen.src);
        Ok(())
    }

    fn export_funcs(
        &mut self,
        resolve: &Resolve,
        world: WorldId,
        funcs: &[(&str, &Function)],
        _files: &mut Files,
    ) -> Result<()> {
        let name = &resolve.worlds[world].name;
        let mut gen = self.interface(resolve, false, None);
        gen.define_function_types(funcs);

        for (i, (_name, func)) in funcs.iter().enumerate() {
            if i == 0 {
                uwriteln!(gen.src.h_fns, "\n// Exported Functions from `{name}`");
                uwriteln!(gen.src.c_fns, "\n// Exported Functions from `{name}`");
            }
            gen.export(func, None);
        }

        gen.gen.src.append(&gen.src);
        Ok(())
    }

    fn import_types(
        &mut self,
        resolve: &Resolve,
        _world: WorldId,
        types: &[(&str, TypeId)],
        _files: &mut Files,
    ) {
        let mut gen = self.interface(resolve, true, Some("$root"));
        let mut live = LiveTypes::default();
        for (_, id) in types {
            live.add_type_id(resolve, *id);
        }
        gen.define_live_types(live);
        gen.gen.src.append(&gen.src);
    }

    fn finish(&mut self, resolve: &Resolve, id: WorldId, files: &mut Files) -> Result<()> {
        let linking_symbol = component_type_object::linking_symbol(&self.world);
        self.c_include("<stdlib.h>");
        let snake = self.world.to_snake_case();
        uwriteln!(
            self.src.c_adapters,
            "\n// Ensure that the *_component_type.o object is linked in"
        );
        uwrite!(
            self.src.c_adapters,
            "
               procedure {linking_symbol}; external;
               procedure {linking_symbol}_public_use_in_this_compilation_unit;
               begin
                 {linking_symbol};
               end;
           ",
        );

        self.print_intrinsics();

        if self.needs_string {
            self.c_include("<string.h>");
            let (strlen, size) = match self.opts.string_encoding {
                StringEncoding::UTF8 => (format!("strlen(s)"), 1),
                StringEncoding::UTF16 => {
                    self.h_include("<uchar.h>");
                    uwrite!(
                        self.src.h_helpers,
                        "
                            size_t {snake}_string_len(const char16_t* s);
                        ",
                    );
                    uwrite!(
                        self.src.c_helpers,
                        "
                            size_t {snake}_string_len(const char16_t* s) {{
                                char16_t* c = (char16_t*)s;
                                for (; *c; ++c);
                                return c-s;
                            }}
                        ",
                    );
                    (format!("{snake}_string_len(s)"), 2)
                }
                StringEncoding::CompactUTF16 => unimplemented!(),
            };
            let ty = self.char_type();
            let c_string_ty = match self.opts.string_encoding {
                StringEncoding::UTF8 => "char",
                StringEncoding::UTF16 => "char16_t",
                StringEncoding::CompactUTF16 => panic!("Compact UTF16 unsupported"),
            };
            uwrite!(
                self.src.h_helpers,
                "
                   // Constructs a string object
                   function {snake}_string_create(ptr: P{c_string_ty}; len: SizeUInt): {snake}_string_t;

                   // Transfers ownership of `s` into the string `ret`
                   procedure {snake}_string_set(ret: P{snake}_string_t; const s: P{c_string_ty});

                   // Creates a copy of the input nul-terminate string `s` and
                   // stores it into the component model string `ret`.
                   procedure {snake}_string_dup(ret: P{snake}_string_t; const s: P{c_string_ty});

                   // Deallocates the string pointed to by `ret`, deallocating
                   // the memory behind the string.
                   procedure {snake}_string_free(ret: P{snake}_string_t);\
               ",
            );
            uwrite!(
                self.src.c_helpers,
                "
                   function {snake}_string_create(ptr: P{c_string_ty}; len: SizeUInt): {snake}_string_t;
                   begin
                     {snake}_string_create.ptr := ptr;
                     {snake}_string_create.len := len;
                   end;

                   procedure {snake}_string_set(ret: P{snake}_string_t; const s: P{c_string_ty});
                   begin
                     ret^.ptr := P{ty}(s);
                     ret^.len := {strlen};
                   end;

                   procedure {snake}_string_dup(ret: P{snake}_string_t; const s: P{c_string_ty});
                   begin
                     ret^.len := {strlen};
                     ret^.ptr := P{ty}(cabi_realloc(nil, 0, {size}, ret^.len * {size}));
                     Move(s^, ret^.ptr^, ret^.len * {size});
                   end;

                   procedure {snake}_string_free(ret: P{snake}_string_t);
                   begin
                     if ret^.len > 0 then
                       FreeMem(ret^.ptr);
                     ret^.ptr := nil;
                     ret^.len := 0;
                   end;
               ",
            );
        }
        if self.needs_union_int32_float {
            uwriteln!(
                self.src.c_helpers,
                "\nunion int32_float {{ int32_t a; float b; }};"
            );
        }
        if self.needs_union_float_int32 {
            uwriteln!(
                self.src.c_helpers,
                "\nunion float_int32 {{ float a; int32_t b; }};"
            );
        }
        if self.needs_union_int64_double {
            uwriteln!(
                self.src.c_helpers,
                "\nunion int64_double {{ int64_t a; double b; }};"
            );
        }
        if self.needs_union_double_int64 {
            uwriteln!(
                self.src.c_helpers,
                "\nunion double_int64 {{ double a; int64_t b; }};"
            );
        }
        let version = env!("CARGO_PKG_VERSION");
        let mut h_str = wit_bindgen_core::Source::default();

        wit_bindgen_core::generated_preamble(&mut h_str, version);

        uwrite!(
            h_str,
            "{{$ifndef __BINDINGS_{0}_H}}
            {{$define __BINDINGS_{0}_H}}
            //#ifdef __cplusplus
            //extern \"C\" {{",
            self.world.to_shouty_snake_case(),
        );

        // Deindent the extern C { declaration
        //h_str.deindent(1);
        uwriteln!(h_str, "\n//#endif\n");

        uwriteln!(h_str, "//#include <stdint.h>");
        uwriteln!(h_str, "//#include <stdbool.h>");
        uwriteln!(h_str, "//#include <stddef.h>");
        for include in self.h_includes.iter() {
            uwriteln!(h_str, "//#include {include}");
        }

        let mut c_str = wit_bindgen_core::Source::default();
        wit_bindgen_core::generated_preamble(&mut c_str, version);
        uwriteln!(c_str, "//#include \"{snake}.h\"");
        for include in self.c_includes.iter() {
            uwriteln!(c_str, "//#include {include}");
        }
        c_str.push_str(&self.src.c_defs);
        c_str.push_str(&self.src.c_fns);

        if self.needs_string {
            uwriteln!(
                h_str,
                "
                type
                  PP{snake}_string_t = ^P{snake}_string_t;
                  P{snake}_string_t = ^{snake}_string_t;
                  {snake}_string_t = record\n\
                  ptr: P{ty};\n\
                  len: SizeUInt;\n\
                end;",
                ty = self.char_type(),
            );
        }
        if self.src.h_defs.len() > 0 {
            h_str.push_str(&self.src.h_defs);
        }

        h_str.push_str(&self.src.h_fns);

        if !self.opts.no_helpers && self.src.h_helpers.len() > 0 {
            uwriteln!(h_str, "\n// Helper Functions");
            h_str.push_str(&self.src.h_helpers);
            h_str.push_str("\n");
        }

        if !self.opts.no_helpers && self.src.c_helpers.len() > 0 {
            uwriteln!(c_str, "\n// Helper Functions");
            c_str.push_str(self.src.c_helpers.as_mut_string());
        }

        uwriteln!(c_str, "\n// Component Adapters");

        // Declare a statically-allocated return area, if needed. We only do
        // this for export bindings, because import bindings allocate their
        // return-area on the stack.
        if self.return_pointer_area_size > 0 {
            // Automatic indentation avoided due to `extern "C" {` declaration
            uwrite!(
                c_str,
                "
                __attribute__((__aligned__({})))
                static uint8_t STATIC_RET_AREA[{}];
                ",
                self.return_pointer_area_align,
                self.return_pointer_area_size,
            );
        }
        c_str.push_str(&self.src.c_adapters);

        uwriteln!(
            h_str,
            "
            //#ifdef __cplusplus
            //}}
            //#endif
            {{$endif}}"
        );

        let mut unit_str = wit_bindgen_core::Source::default();
        wit_bindgen_core::generated_preamble(&mut unit_str, version);
        uwriteln!(
            unit_str,
            "unit {snake};
              {{$PACKRECORDS C}}
            interface
              {{$I {snake}h.inc}}
            implementation
              {{$I {snake}.inc}}
            end.");

        files.push(&format!("{snake}.pas"), unit_str.as_bytes());
        files.push(&format!("{snake}h.inc"), h_str.as_bytes());
        files.push(&format!("{snake}.inc"), c_str.as_bytes());
        if !self.opts.no_object_file {
            files.push(
                &format!("{snake}_component_type.o",),
                component_type_object::object(
                    resolve,
                    id,
                    &self.world,
                    self.opts.string_encoding,
                    self.opts.type_section_suffix.as_deref(),
                )
                .unwrap()
                .as_slice(),
            );
        }

        Ok(())
    }

    fn pre_export_interface(&mut self, resolve: &Resolve, _files: &mut Files) -> Result<()> {
        self.remove_types_redefined_by_exports(resolve, self.world_id.unwrap());
        Ok(())
    }
}

impl Pascal {
    fn interface<'a>(
        &'a mut self,
        resolve: &'a Resolve,
        in_import: bool,
        wasm_import_module: Option<&'a str>,
    ) -> InterfaceGenerator<'a> {
        InterfaceGenerator {
            src: Source::default(),
            gen: self,
            resolve,
            interface: None,
            in_import,
            wasm_import_module,
        }
    }

    fn h_include(&mut self, s: &str) {
        self.h_includes.push(s.to_string());
    }

    fn c_include(&mut self, s: &str) {
        self.c_includes.push(s.to_string());
    }

    fn char_type(&self) -> &'static str {
        match self.opts.string_encoding {
            StringEncoding::UTF8 => "char",
            StringEncoding::UTF16 => "widechar",
            StringEncoding::CompactUTF16 => panic!("Compact UTF16 unsupported"),
        }
    }

    fn type_name(&mut self, ty: &Type) -> String {
        let mut name = String::new();
        self.push_type_name(ty, &mut name);
        name
    }

    fn push_type_name(&mut self, ty: &Type, dst: &mut String) {
        match ty {
            Type::Bool => dst.push_str("boolean"),
            Type::Char => dst.push_str("uint32"), // TODO: better type?
            Type::U8 => dst.push_str("byte"),
            Type::S8 => dst.push_str("int8"),
            Type::U16 => dst.push_str("uint16"),
            Type::S16 => dst.push_str("int16"),
            Type::U32 => dst.push_str("uint32"),
            Type::S32 => dst.push_str("int32"),
            Type::U64 => dst.push_str("uint64"),
            Type::S64 => dst.push_str("int64"),
            Type::F32 => dst.push_str("single"),
            Type::F64 => dst.push_str("double"),
            Type::String => {
                dst.push_str(&self.world.to_snake_case());
                dst.push_str("_");
                dst.push_str("string_t");
                self.needs_string = true;
            }
            Type::Id(id) => {
                if let Some(name) = self.type_names.get(id) {
                    dst.push_str(name);
                    return;
                }

                panic!("failed to find type name for {id:?}");
            }
        }
    }

    /// Removes all types from `self.{dtor_funcs,type_names,resources}` which
    /// are redefined in exports.
    ///
    /// WIT interfaces can be both imported and exported but they're represented
    /// with the same `TypeId` internally within the `wit-parser`
    /// representation. This means that duplicate types need to be generated for
    /// exports, even if the same interface was already imported. If nothing
    /// were done here though then the same type imported and exported wouldn't
    /// generate anything new since preexisting types are skipped in
    /// `define_live_types`.
    ///
    /// This function will trim the sets on `self` to only retain those types
    /// which exports refer to that come from imports.
    fn remove_types_redefined_by_exports(&mut self, resolve: &Resolve, world: WorldId) {
        let live_import_types = imported_types_used_by_exported_interfaces(resolve, world);
        self.dtor_funcs.retain(|k, _| live_import_types.contains(k));
        self.type_names.retain(|k, _| live_import_types.contains(k));
        self.resources.retain(|k, _| live_import_types.contains(k));
    }

    fn perform_cast(&mut self, op: &str, cast: &Bitcast) -> String {
        match cast {
            Bitcast::I32ToF32 | Bitcast::I64ToF32 => {
                self.needs_union_int32_float = true;
                format!("((union int32_float){{ (int32_t) {} }}).b", op)
            }
            Bitcast::F32ToI32 | Bitcast::F32ToI64 => {
                self.needs_union_float_int32 = true;
                format!("((union float_int32){{ {} }}).b", op)
            }
            Bitcast::I64ToF64 => {
                self.needs_union_int64_double = true;
                format!("((union int64_double){{ (int64_t) {} }}).b", op)
            }
            Bitcast::F64ToI64 => {
                self.needs_union_double_int64 = true;
                format!("((union double_int64){{ {} }}).b", op)
            }
            Bitcast::I32ToI64 | Bitcast::LToI64 | Bitcast::PToP64 => {
                format!("int64({})", op)
            }
            Bitcast::I64ToI32 | Bitcast::I64ToL => {
                format!("int32({})", op)
            }
            // P64 is currently represented as int64_t, so no conversion is needed.
            Bitcast::I64ToP64 | Bitcast::P64ToI64 => {
                format!("{}", op)
            }
            Bitcast::P64ToP | Bitcast::I32ToP | Bitcast::LToP => {
                format!("(uint8_t *) {}", op)
            }

            // Cast to uintptr_t to avoid implicit pointer-to-int conversions.
            Bitcast::PToI32 | Bitcast::PToL => format!("(uintptr_t) {}", op),

            Bitcast::I32ToL | Bitcast::LToI32 | Bitcast::None => op.to_string(),

            Bitcast::Sequence(sequence) => {
                let [first, second] = &**sequence;
                let inner = self.perform_cast(op, first);
                self.perform_cast(&inner, second)
            }
        }
    }
}

pub fn imported_types_used_by_exported_interfaces(
    resolve: &Resolve,
    world: WorldId,
) -> HashSet<TypeId> {
    // First build up a set of all types used by exports and all the
    // exported interfaces.
    let mut live_export_types = LiveTypes::default();
    let mut exported_interfaces = HashSet::new();
    for (_, export) in resolve.worlds[world].exports.iter() {
        match export {
            WorldItem::Function(_) => {}
            WorldItem::Interface { id, .. } => {
                exported_interfaces.insert(*id);
                live_export_types.add_interface(resolve, *id)
            }
            WorldItem::Type(_) => unreachable!(),
        }
    }

    // Using the above sets a set of required import interfaces can be
    // calculated. This is all referred-to-types that are owned by an
    // interface that aren't present in an export. Note that the topological
    // sorting and WIT requirements are what makes this check possible.
    let mut imports_used = HashSet::new();
    for ty in live_export_types.iter() {
        if let TypeOwner::Interface(id) = resolve.types[ty].owner {
            if !exported_interfaces.contains(&id) {
                imports_used.insert(id);
            }
        }
    }

    // With the set of imports used that aren't shadowed by exports the set
    // of types on `self` can now be trimmed. All live types in all the
    // imports are calculated and then everything except these are removed.
    let mut live_import_types = LiveTypes::default();
    for import in imports_used {
        live_import_types.add_interface(resolve, import);
    }
    let live_import_types = live_import_types.iter().collect::<HashSet<_>>();
    live_import_types
}

fn is_prim_type(resolve: &Resolve, ty: &Type) -> bool {
    if let Type::Id(id) = ty {
        is_prim_type_id(resolve, *id)
    } else {
        true
    }
}

fn is_prim_type_id(resolve: &Resolve, id: TypeId) -> bool {
    match &resolve.types[id].kind {
        TypeDefKind::List(elem) => is_prim_type(resolve, elem),

        TypeDefKind::Option(ty) => is_prim_type(resolve, ty),

        TypeDefKind::Tuple(tuple) => tuple.types.iter().all(|ty| is_prim_type(resolve, ty)),

        TypeDefKind::Type(ty) => is_prim_type(resolve, ty),

        TypeDefKind::Record(_)
        | TypeDefKind::Resource
        | TypeDefKind::Handle(_)
        | TypeDefKind::Flags(_)
        | TypeDefKind::Variant(_)
        | TypeDefKind::Enum(_)
        | TypeDefKind::Result(_)
        | TypeDefKind::Future(_)
        | TypeDefKind::Stream(_)
        | TypeDefKind::ErrorContext
        | TypeDefKind::Unknown => false,
    }
}

pub fn push_ty_name(resolve: &Resolve, ty: &Type, src: &mut String) {
    match ty {
        Type::Bool => src.push_str("bool"),
        Type::Char => src.push_str("char32"),
        Type::U8 => src.push_str("u8"),
        Type::S8 => src.push_str("s8"),
        Type::U16 => src.push_str("u16"),
        Type::S16 => src.push_str("s16"),
        Type::U32 => src.push_str("u32"),
        Type::S32 => src.push_str("s32"),
        Type::U64 => src.push_str("u64"),
        Type::S64 => src.push_str("s64"),
        Type::F32 => src.push_str("f32"),
        Type::F64 => src.push_str("f64"),
        Type::String => src.push_str("string"),
        Type::Id(id) => {
            let ty = &resolve.types[*id];
            if let Some(name) = &ty.name {
                return src.push_str(&name.to_snake_case());
            }
            match &ty.kind {
                TypeDefKind::Type(t) => push_ty_name(resolve, t, src),
                TypeDefKind::Record(_)
                | TypeDefKind::Resource
                | TypeDefKind::Flags(_)
                | TypeDefKind::Enum(_)
                | TypeDefKind::Variant(_) => {
                    unimplemented!()
                }
                TypeDefKind::Tuple(t) => {
                    src.push_str("tuple");
                    src.push_str(&t.types.len().to_string());
                    for ty in t.types.iter() {
                        src.push_str("_");
                        push_ty_name(resolve, ty, src);
                    }
                }
                TypeDefKind::Option(ty) => {
                    src.push_str("option_");
                    push_ty_name(resolve, ty, src);
                }
                TypeDefKind::Result(r) => {
                    src.push_str("result_");
                    match &r.ok {
                        Some(ty) => push_ty_name(resolve, ty, src),
                        None => src.push_str("void"),
                    }
                    src.push_str("_");
                    match &r.err {
                        Some(ty) => push_ty_name(resolve, ty, src),
                        None => src.push_str("void"),
                    }
                }
                TypeDefKind::List(ty) => {
                    src.push_str("list_");
                    push_ty_name(resolve, ty, src);
                }
                TypeDefKind::Future(_) => todo!(),
                TypeDefKind::Stream(_) => todo!(),
                TypeDefKind::ErrorContext => todo!(),
                TypeDefKind::Handle(Handle::Own(resource)) => {
                    src.push_str("own_");
                    push_ty_name(resolve, &Type::Id(*resource), src);
                }
                TypeDefKind::Handle(Handle::Borrow(resource)) => {
                    src.push_str("borrow_");
                    push_ty_name(resolve, &Type::Id(*resource), src);
                }
                TypeDefKind::Unknown => unreachable!(),
            }
        }
    }
}

pub fn owner_namespace<'a>(
    interface: Option<(InterfaceId, &'a WorldKey)>,
    in_import: bool,
    world: String,
    resolve: &Resolve,
    id: TypeId,
    renamed_interfaces: &HashMap<WorldKey, String>,
) -> String {
    let ty = &resolve.types[id];
    match (ty.owner, interface) {
        // If this type is owned by an interface, then we must be generating
        // bindings for that interface to proceed.
        (TypeOwner::Interface(a), Some((b, key))) if a == b => {
            interface_identifier(key, resolve, !in_import, renamed_interfaces)
        }
        (TypeOwner::Interface(_), None) => unreachable!(),
        (TypeOwner::Interface(_), Some(_)) => unreachable!(),

        // If this type is owned by a world then we must not be generating
        // bindings for an interface.
        (TypeOwner::World(_), None) => world.to_snake_case(),
        (TypeOwner::World(_), Some(_)) => unreachable!(),

        // If this type has no owner then it's an anonymous type. Here it's
        // assigned to whatever we happen to be generating bindings for.
        (TypeOwner::None, Some((_, key))) => {
            interface_identifier(key, resolve, !in_import, renamed_interfaces)
        }
        (TypeOwner::None, None) => world.to_snake_case(),
    }
}

fn interface_identifier(
    interface_id: &WorldKey,
    resolve: &Resolve,
    in_export: bool,
    renamed_interfaces: &HashMap<WorldKey, String>,
) -> String {
    if let Some(rename) = renamed_interfaces.get(interface_id) {
        let mut ns = String::new();
        if in_export && matches!(interface_id, WorldKey::Interface(_)) {
            ns.push_str("exports_");
        }
        ns.push_str(rename);
        return ns;
    }

    match interface_id {
        WorldKey::Name(name) => name.to_snake_case(),
        WorldKey::Interface(id) => {
            let mut ns = String::new();
            if in_export {
                ns.push_str("exports_");
            }
            let iface = &resolve.interfaces[*id];
            let pkg = &resolve.packages[iface.package.unwrap()];
            ns.push_str(&pkg.name.namespace.to_snake_case());
            ns.push_str("_");
            ns.push_str(&pkg.name.name.to_snake_case());
            ns.push_str("_");
            let pkg_has_multiple_versions = resolve.packages.iter().any(|(_, p)| {
                p.name.namespace == pkg.name.namespace
                    && p.name.name == pkg.name.name
                    && p.name.version != pkg.name.version
            });
            if pkg_has_multiple_versions {
                if let Some(version) = &pkg.name.version {
                    let version = version
                        .to_string()
                        .replace('.', "_")
                        .replace('-', "_")
                        .replace('+', "_");
                    ns.push_str(&version);
                    ns.push_str("_");
                }
            }
            ns.push_str(&iface.name.as_ref().unwrap().to_snake_case());
            ns
        }
    }
}

pub fn c_func_name(
    in_import: bool,
    resolve: &Resolve,
    world: &str,
    interface_id: Option<&WorldKey>,
    func: &Function,
    renamed_interfaces: &HashMap<WorldKey, String>,
) -> String {
    let mut name = String::new();
    match interface_id {
        Some(id) => name.push_str(&interface_identifier(
            id,
            resolve,
            !in_import,
            renamed_interfaces,
        )),
        None => {
            if !in_import {
                name.push_str("exports_");
            }
            name.push_str(&world.to_snake_case());
        }
    }
    name.push_str("_");
    name.push_str(&func.name.to_snake_case().replace('.', "_"));
    name
}

struct InterfaceGenerator<'a> {
    src: Source,
    in_import: bool,
    gen: &'a mut Pascal,
    resolve: &'a Resolve,
    interface: Option<(InterfaceId, &'a WorldKey)>,
    wasm_import_module: Option<&'a str>,
}

impl Pascal {
    fn print_intrinsics(&mut self) {
        // Note that these intrinsics are declared as `weak` so they can be
        // overridden from some other symbol.
        self.src.c_fns("\n// Canonical ABI intrinsics");
        self.src.c_fns("\n");
        self.src.c_fns(
            r#"
                //__attribute__((__weak__, __export_name__("cabi_realloc")))
                function cabi_realloc(ptr: Pointer; old_size: SizeUInt; align: SizeUInt; new_size: SizeUInt): Pointer;
                begin
                  if new_size = 0 then
                  begin
                    cabi_realloc := Pointer(align);
                    exit;
                  end;
                  ReallocMem(ptr, new_size);
                  //if (!ptr) abort();
                  cabi_realloc := ptr;
                end;
            "#,
        );
    }
}

impl Return {
    fn return_single(
        &mut self,
        resolve: &Resolve,
        ty: &Type,
        orig_ty: &Type,
        sig_flattening: bool,
    ) {
        let id = match ty {
            Type::Id(id) => *id,
            Type::String => {
                self.retptrs.push(*orig_ty);
                return;
            }
            _ => {
                self.scalar = Some(Scalar::Type(*orig_ty));
                return;
            }
        };
        match &resolve.types[id].kind {
            TypeDefKind::Type(t) => return self.return_single(resolve, t, orig_ty, sig_flattening),

            // Flags are returned as their bare values, and enums and handles are scalars
            TypeDefKind::Flags(_) | TypeDefKind::Enum(_) | TypeDefKind::Handle(_) => {
                self.scalar = Some(Scalar::Type(*orig_ty));
                return;
            }

            // Unpack optional returns where a boolean discriminant is
            // returned and then the actual type returned is returned
            // through a return pointer.
            TypeDefKind::Option(ty) => {
                if sig_flattening {
                    self.scalar = Some(Scalar::OptionBool(*ty));
                    self.retptrs.push(*ty);
                    return;
                }
            }

            // Unpack a result as a boolean return type, with two
            // return pointers for ok and err values
            TypeDefKind::Result(r) => {
                if sig_flattening {
                    if let Some(ok) = r.ok {
                        self.retptrs.push(ok);
                    }
                    if let Some(err) = r.err {
                        self.retptrs.push(err);
                    }
                    self.scalar = Some(Scalar::ResultBool(r.ok, r.err));
                    return;
                }
            }

            // These types are always returned indirectly.
            TypeDefKind::Tuple(_)
            | TypeDefKind::Record(_)
            | TypeDefKind::List(_)
            | TypeDefKind::Variant(_) => {}

            TypeDefKind::Future(_) => todo!("return_single for future"),
            TypeDefKind::Stream(_) => todo!("return_single for stream"),
            TypeDefKind::ErrorContext => todo!("return_single for error-context"),
            TypeDefKind::Resource => todo!("return_single for resource"),
            TypeDefKind::Unknown => unreachable!(),
        }

        self.retptrs.push(*orig_ty);
    }
}

impl<'a> wit_bindgen_core::InterfaceGenerator<'a> for InterfaceGenerator<'a> {
    fn resolve(&self) -> &'a Resolve {
        self.resolve
    }

    fn type_record(&mut self, id: TypeId, _name: &str, record: &Record, docs: &Docs) {
        self.src.h_defs("\n");
        self.docs(docs, SourceType::HDefs);
        self.start_typedef_struct(id);
        for field in record.fields.iter() {
            self.docs(&field.docs, SourceType::HDefs);
            self.src.h_defs(&to_pascal_ident(&field.name));
            self.src.h_defs(": ");
            self.print_ty(SourceType::HDefs, &field.ty);
            self.src.h_defs(";\n");
        }
        self.finish_typedef_struct(id);
    }

    fn type_resource(&mut self, id: TypeId, name: &str, _docs: &Docs) {
        let ns = self.owner_namespace(id);
        let snake = name.to_snake_case();
        let mut own = ns.clone();
        let mut borrow = own.clone();
        own.push_str("_own");
        borrow.push_str("_borrow");
        own.push_str("_");
        borrow.push_str("_");
        own.push_str(&snake);
        borrow.push_str(&snake);
        own.push_str("_t");
        borrow.push_str("_t");

        // All resources, whether or not they're imported or exported, get the
        // ability to drop handles.
        self.src.h_helpers(&format!(
            "
procedure {ns}_{snake}_drop_own(handle: {own});
            "
        ));
        let import_module = if self.in_import {
            self.wasm_import_module.unwrap().to_string()
        } else {
            let module = match self.interface {
                Some((_, key)) => self.resolve.name_world_key(key),
                None => unimplemented!("resource exports from worlds"),
            };
            format!("[export]{module}")
        };

        let drop_fn = format!("__wasm_import_{ns}_{snake}_drop");

        self.src.c_helpers(&format!(
            r#"
procedure {drop_fn}(handle: int32); external '{import_module}' name '[resource-drop]{name}';

procedure {ns}_{snake}_drop_own(handle: {own});
begin
  {drop_fn}(handle.__handle);
end;
            "#
        ));

        // All resources, whether or not they're imported or exported, have an
        // handle-index-based representation for "own" handles.
        self.src.h_defs(&format!(
            "
            type
              PP{own} = ^P{own};
              P{own} = ^{own};
              {own} = record
                __handle: int32;
              end;
            "
        ));

        if self.in_import {
            // For imported resources borrowed handles are represented the same
            // way as owned handles. They're given a unique type, however, to
            // prevent type confusion at runtime in theory.
            self.src.h_defs(&format!(
                "
                type
                  PP{borrow} = ^P{borrow};
                  P{borrow} = ^{borrow};
                  {borrow} = record
                    __handle: int32;
                  end;
                "
            ));

            if self.autodrop_enabled() {
                // As we have two different types for owned vs borrowed resources,
                // but owns and borrows are dropped using the same intrinsic we
                // also generate a version of the drop function for borrows that we
                // possibly acquire through our exports.
                self.src.h_helpers(&format!(
                    "\nprocedure {ns}_{snake}_drop_borrow(handle: {borrow});\n"
                ));

                self.src.c_helpers(&format!(
                    "
procedure {ns}_{snake}_drop_borrow(handle: {borrow});
begin
  __wasm_import_{ns}_{snake}_drop(handle.__handle);
end;
                "
                ));
            }

            // To handle the two types generated for borrow/own this helper
            // function enables converting an own handle to a borrow handle
            // which will have the same index internally.
            self.src.h_helpers(&format!(
                "
function {ns}_borrow_{snake}(handle: {own}): {borrow};
                "
            ));

            self.src.c_helpers(&format!(
                r#"
function {ns}_borrow_{snake}(handle: {own}): {borrow};
begin
  {ns}_borrow_{snake} := {borrow}( handle.__handle );
end;
                "#
            ));
        } else {
            // For exported resources first generate a typedef that the user
            // will be required to fill in. This is an empty struct.
            self.src.h_defs("\n");
            self.src.h_defs("typedef struct ");
            let ty_name = self.gen.type_names[&id].clone();
            self.src.h_defs(&ty_name);
            self.src.h_defs(" ");
            self.print_typedef_target(id);
            let (_, key) = self.interface.unwrap();
            let module = self.resolve.name_world_key(key);

            // Exported resources use a different representation than imports
            // for borrows which is a raw pointer to the struct declared just
            // above.
            self.src
                .h_defs(&format!("\ntypedef {ty_name}* {borrow};\n"));

            // Exported resources are defined by this module which means they
            // get access to more intrinsics:
            //
            // * construction of a resource (rep to handle)
            // * extraction of the representation of a resource (handle to rep)
            //
            // Additionally users must define a destructor for this resource, so
            // declare its prototype here.
            self.src.h_helpers(&format!(
                "
extern {own} {ns}_{snake}_new({ty_name} *rep);
extern {ty_name}* {ns}_{snake}_rep({own} handle);
void {ns}_{snake}_destructor({ty_name} *rep);
                "
            ));

            self.src.c_helpers(&format!(
                r#"
__attribute__(( __import_module__("[export]{module}"), __import_name__("[resource-new]{name}")))
extern int32_t __wasm_import_{ns}_{snake}_new(int32_t);

__attribute__((__import_module__("[export]{module}"), __import_name__("[resource-rep]{name}")))
extern int32_t __wasm_import_{ns}_{snake}_rep(int32_t);

{own} {ns}_{snake}_new({ty_name} *rep) {{
    return ({own}) {{ __wasm_import_{ns}_{snake}_new((int32_t) rep) }};
}}

{ty_name}* {ns}_{snake}_rep({own} handle) {{
    return ({ns}_{snake}_t*) __wasm_import_{ns}_{snake}_rep(handle.__handle);
}}

__attribute__((__export_name__("{module}#[dtor]{snake}")))
void __wasm_export_{ns}_{snake}_dtor({ns}_{snake}_t* arg) {{
    {ns}_{snake}_destructor(arg);
}}
                "#
            ));
        }

        self.gen.resources.insert(
            id,
            ResourceInfo {
                own,
                borrow,
                direction: if self.in_import {
                    Direction::Import
                } else {
                    Direction::Export
                },
                drop_fn,
            },
        );
    }

    fn type_tuple(&mut self, id: TypeId, _name: &str, tuple: &Tuple, docs: &Docs) {
        self.src.h_defs("\n");
        self.docs(docs, SourceType::HDefs);
        self.start_typedef_struct(id);
        for (i, ty) in tuple.types.iter().enumerate() {
            uwrite!(self.src.h_defs, " f{i}: ");
            self.print_ty(SourceType::HDefs, ty);
            uwriteln!(self.src.h_defs, ";");
        }
        self.finish_typedef_struct(id);
    }

    fn type_flags(&mut self, id: TypeId, name: &str, flags: &Flags, docs: &Docs) {
        self.src.h_defs("\n");
        self.docs(docs, SourceType::HDefs);
        let repr = flags_repr(flags);
        let int_t = int_repr(repr);

        uwriteln!(
            self.src.h_defs,
            "type
              PP{0} = ^P{0};
              P{0} = ^{0};
              {0} = {int_t};",
            &self.gen.type_names[&id],
        );

        if flags.flags.len() > 0 {
            self.src.h_defs("\n");
        }
        let ns = self.owner_namespace(id).to_shouty_snake_case();
        for (i, flag) in flags.flags.iter().enumerate() {
            self.docs(&flag.docs, SourceType::HDefs);
            uwriteln!(
                self.src.h_defs,
                "const {ns}_{}_{} = 1 shl {i};",
                name.to_shouty_snake_case(),
                flag.name.to_shouty_snake_case(),
            );
        }
    }

    fn type_variant(&mut self, id: TypeId, name: &str, variant: &Variant, docs: &Docs) {
        let cases_with_data = Vec::from_iter(
            variant
                .cases
                .iter()
                .filter_map(|case| case.ty.as_ref().map(|ty| (&case.name, ty))),
        );

        self.src.h_defs("\n");

        let ns = self.owner_namespace(id).to_shouty_snake_case();
        for (i, case) in variant.cases.iter().enumerate() {
            self.docs(&case.docs, SourceType::HDefs);
            uwriteln!(
                self.src.h_defs,
                "const {ns}_{}_{} = {i};",
                name.to_shouty_snake_case(),
                case.name.to_shouty_snake_case(),
            );
        }

        self.docs(docs, SourceType::HDefs);
        self.start_typedef_struct(id);
        if !cases_with_data.is_empty() {
            self.src.h_defs("case ");
        }
        self.src.h_defs("tag: ");
        self.src.h_defs(int_repr(variant.tag()));
        if !cases_with_data.is_empty() {
            self.src.h_defs(" of\n");
        } else {
            self.src.h_defs(";\n");
        }

        if !cases_with_data.is_empty() {
            for (case_name, ty) in cases_with_data {
                self.src.h_defs(&format!("{ns}_{}_{}: (", name.to_shouty_snake_case(), case_name.to_shouty_snake_case()));
                self.src.h_defs(&to_pascal_ident(case_name));
                self.src.h_defs(": ");
                self.print_ty(SourceType::HDefs, ty);
                self.src.h_defs(");\n");
            }
        }
        self.finish_typedef_struct(id);

        if variant.cases.len() > 0 {
            self.src.h_defs("\n");
        }
    }

    fn type_option(&mut self, id: TypeId, _name: &str, payload: &Type, docs: &Docs) {
        self.src.h_defs("\n");
        self.docs(docs, SourceType::HDefs);
        self.start_typedef_struct(id);
        self.src.h_defs("bool is_some;\n");
        self.print_ty(SourceType::HDefs, payload);
        self.src.h_defs(" val;\n");
        self.finish_typedef_struct(id);
    }

    fn type_result(&mut self, id: TypeId, _name: &str, result: &Result_, docs: &Docs) {
        self.src.h_defs("\n");
        self.docs(docs, SourceType::HDefs);
        self.start_typedef_struct(id);
        self.src.h_defs("bool is_err;\n");
        if result.ok.is_some() || result.err.is_some() {
            self.src.h_defs("union {\n");
            if let Some(ok) = result.ok.as_ref() {
                self.print_ty(SourceType::HDefs, ok);
                self.src.h_defs(" ok;\n");
            }
            if let Some(err) = result.err.as_ref() {
                self.print_ty(SourceType::HDefs, err);
                self.src.h_defs(" err;\n");
            }
            self.src.h_defs("} val;\n");
        }
        self.finish_typedef_struct(id);
    }

    fn type_enum(&mut self, id: TypeId, name: &str, enum_: &Enum, docs: &Docs) {
        uwrite!(self.src.h_defs, "\n");
        self.docs(docs, SourceType::HDefs);
        let int_t = int_repr(enum_.tag());
        uwriteln!(
            self.src.h_defs,
            "type
              PP{0} = ^P{0};
              P{0} = ^{0};
              {0} = {int_t};",
            &self.gen.type_names[&id],
        );

        if enum_.cases.len() > 0 {
            self.src.h_defs("\n");
        }
        let ns = self.owner_namespace(id).to_shouty_snake_case();
        for (i, case) in enum_.cases.iter().enumerate() {
            self.docs(&case.docs, SourceType::HDefs);
            uwriteln!(
                self.src.h_defs,
                "const {ns}_{}_{} = {i};",
                name.to_shouty_snake_case(),
                case.name.to_shouty_snake_case(),
            );
        }
    }

    fn type_alias(&mut self, id: TypeId, _name: &str, ty: &Type, docs: &Docs) {
        // we should skip generating `typedef` for `Resource` types because they aren't even
        // defined anywhere, not even in `type_resource`. Only its `Handle` types are defined.
        // The aliasing handle types are defined in `define_anonymous_type`.
        let target = dealias(self.resolve, id);
        if matches!(&self.resolve.types[target].kind, TypeDefKind::Resource) {
            return;
        }

        self.src.h_defs("\n");
        self.docs(docs, SourceType::HDefs);

        uwrite!(
            self.src.h_defs,
            "
            type
              PP{0} = ^P{0};
              P{0} = ^{0};
              {0} = ",
            &self.gen.type_names[&id],
        );
        self.print_ty(SourceType::HDefs, ty);
        self.src.h_defs(";\n");
    }

    fn type_list(&mut self, id: TypeId, _name: &str, ty: &Type, docs: &Docs) {
        self.src.h_defs("\n");
        self.docs(docs, SourceType::HDefs);
        self.start_typedef_struct(id);
        self.print_ty(SourceType::HDefs, ty);
        self.src.h_defs(" *ptr;\n");
        self.src.h_defs("size_t len;\n");
        self.finish_typedef_struct(id);
    }

    fn type_future(&mut self, id: TypeId, name: &str, ty: &Option<Type>, docs: &Docs) {
        _ = (id, name, ty, docs);
        todo!()
    }

    fn type_stream(&mut self, id: TypeId, name: &str, ty: &Option<Type>, docs: &Docs) {
        _ = (id, name, ty, docs);
        todo!()
    }

    fn type_error_context(&mut self, id: TypeId, name: &str, docs: &Docs) {
        _ = (id, name, docs);
        todo!()
    }

    fn type_builtin(&mut self, id: TypeId, name: &str, ty: &Type, docs: &Docs) {
        let _ = (id, name, ty, docs);
    }
}

impl<'a> wit_bindgen_core::AnonymousTypeGenerator<'a> for InterfaceGenerator<'a> {
    fn resolve(&self) -> &'a Resolve {
        self.resolve
    }

    fn anonymous_type_handle(&mut self, id: TypeId, handle: &Handle, _docs: &Docs) {
        uwrite!(
            self.src.h_defs,
            "
            type
              PP{0} = ^P{0};
              P{0} = ^{0};
              {0} = ",
            &self.gen.type_names[&id]
        );
        let resource = match handle {
            Handle::Borrow(id) | Handle::Own(id) => id,
        };
        let info = &self.gen.resources[&dealias(self.resolve, *resource)];
        match handle {
            Handle::Borrow(_) => self.src.h_defs(&info.borrow),
            Handle::Own(_) => self.src.h_defs(&info.own),
        }
        self.src.h_defs(";");
    }

    fn anonymous_type_tuple(&mut self, id: TypeId, ty: &Tuple, _docs: &Docs) {
        uwriteln!(
            self.src.h_defs,
            "
            type
              PP{0} = ^P{0};
              P{0} = ^{0};
              {0} = record",
            &self.gen.type_names[&id]
        );
        for (i, t) in ty.types.iter().enumerate() {
            let ty = self.gen.type_name(t);
            uwriteln!(self.src.h_defs, "f{i}: {ty};");
        }
        self.src.h_defs("end;\n");
    }

    fn anonymous_type_option(&mut self, id: TypeId, ty: &Type, _docs: &Docs) {
        let ty = self.gen.type_name(ty);
        uwriteln!(
            self.src.h_defs,
            "
            type
              PP{0} = ^P{0};
              P{0} = ^{0};
              {0} = record
                is_some: Boolean;
                val: {ty};
              end;",
            &self.gen.type_names[&id]
        );
    }

    fn anonymous_type_result(&mut self, id: TypeId, ty: &Result_, _docs: &Docs) {
        uwriteln!(
            self.src.h_defs,
            "
            type
              PP{0} = ^P{0};
              P{0} = ^{0};
              {0} = record",
            &self.gen.type_names[&id]
        );
        let ok_ty = ty.ok.as_ref();
        let err_ty = ty.err.as_ref();
        if ok_ty.is_some() || err_ty.is_some() {
            self.src.h_defs("case is_err: Boolean of\n");
            if let Some(ok) = ok_ty {
                let ty = self.gen.type_name(ok);
                uwriteln!(self.src.h_defs, "false: (ok: {ty});");
            }
            if let Some(err) = err_ty {
                let ty = self.gen.type_name(err);
                uwriteln!(self.src.h_defs, "true: (err: {ty});");
            }
        } else {
            self.src.h_defs("is_err: Boolean;\n");
        }
        self.src.h_defs("end;");
    }

    fn anonymous_type_list(&mut self, id: TypeId, ty: &Type, _docs: &Docs) {
        uwriteln!(
            self.src.h_defs,
            "
            type
              PP{0} = ^P{0};
              P{0} = ^{0};
              {0} = record",
            &self.gen.type_names[&id]
        );
        let ty = self.gen.type_name(ty);
        uwriteln!(self.src.h_defs, "  ptr: P{ty};");
        self.src.h_defs("  len: SizeUInt;\n");
        self.src.h_defs("end;");
    }

    fn anonymous_type_future(&mut self, _id: TypeId, _ty: &Option<Type>, _docs: &Docs) {
        todo!("print_anonymous_type for future");
    }

    fn anonymous_type_stream(&mut self, _id: TypeId, _ty: &Option<Type>, _docs: &Docs) {
        todo!("print_anonymous_type for stream");
    }

    fn anonymous_type_error_context(&mut self) {
        todo!("print_anonymous_type for error-context");
    }

    fn anonymous_type_type(&mut self, _id: TypeId, _ty: &Type, _docs: &Docs) {
        todo!("print_anonymous_type for type");
    }
}

pub enum CTypeNameInfo<'a> {
    Named { name: &'a str },
    Anonymous { is_prim: bool },
}

/// Generate the type part of a c identifier, missing the namespace and the `_t` suffix.
/// Additionally return a `CTypeNameInfo` that describes what sort of name has been produced.
pub fn gen_type_name(resolve: &Resolve, ty: TypeId) -> (CTypeNameInfo<'_>, String) {
    let mut encoded = String::new();
    push_ty_name(resolve, &Type::Id(ty), &mut encoded);
    let info = if let Some(name) = &resolve.types[ty].name {
        CTypeNameInfo::Named {
            name: name.as_ref(),
        }
    } else {
        CTypeNameInfo::Anonymous {
            is_prim: is_prim_type_id(resolve, ty),
        }
    };

    (info, encoded)
}

impl InterfaceGenerator<'_> {
    fn define_interface_types(&mut self, id: InterfaceId) {
        let mut live = LiveTypes::default();
        live.add_interface(self.resolve, id);
        self.define_live_types(live);
    }

    fn define_function_types(&mut self, funcs: &[(&str, &Function)]) {
        let mut live = LiveTypes::default();
        for (_, func) in funcs {
            live.add_func(self.resolve, func);
        }
        self.define_live_types(live);
    }

    fn define_live_types(&mut self, live: LiveTypes) {
        for ty in live.iter() {
            if self.gen.type_names.contains_key(&ty) {
                continue;
            }

            let (info, encoded) = gen_type_name(&self.resolve, ty);
            match info {
                CTypeNameInfo::Named { name } => {
                    let typedef_name = format!("{}_{encoded}_t", self.owner_namespace(ty));
                    let prev = self.gen.type_names.insert(ty, typedef_name.clone());
                    assert!(prev.is_none());

                    self.define_type(name, ty)
                }

                CTypeNameInfo::Anonymous { is_prim } => {
                    let (defined, name) = if is_prim {
                        let namespace = self.gen.world.to_snake_case();
                        let name = format!("{namespace}_{encoded}_t");
                        let new_prim = self.gen.prim_names.insert(name.clone());
                        (!new_prim, name)
                    } else {
                        let namespace = self.owner_namespace(ty);
                        (false, format!("{namespace}_{encoded}_t"))
                    };

                    let prev = self.gen.type_names.insert(ty, name);
                    assert!(prev.is_none());

                    if defined {
                        continue;
                    }

                    let kind = &self.resolve.types[ty].kind;
                    if let TypeDefKind::Handle(handle) = kind {
                        let resource = match handle {
                            Handle::Borrow(id) | Handle::Own(id) => id,
                        };
                        let origin = dealias(self.resolve, *resource);
                        if origin == *resource {
                            continue;
                        }
                    }

                    self.define_anonymous_type(ty)
                }
            }

            self.define_constructor(ty);
            self.define_dtor(ty);
        }
    }

    fn define_constructor(&mut self, id: TypeId) {
        //let h_helpers_start = self.src.h_helpers.len();
        //let c_helpers_start = self.src.c_helpers.len();

        let name = self.gen.type_names[&id].clone();
        let prefix = name.strip_suffix("_t").unwrap();

        //self.src
        //    .h_helpers(&format!("\nprocedure {prefix}_create(ptr: P{name});\n"));
        //self.src
        //    .c_helpers(&format!("\nprocedure {prefix}_create(ptr: P{name});\n"));
        //self.src.c_helpers("begin\n");
        //let c_helpers_body_start = self.src.c_helpers.len();
        match &self.resolve.types[id].kind {
            TypeDefKind::Type(t) => {}//self.free(t, "ptr"),

            TypeDefKind::Flags(_) => {}
            TypeDefKind::Enum(_) => {}

            TypeDefKind::Record(r) => {
                let mut params = String::new();
                for field in r.fields.iter() {
                    params.push_str(&format!("const a{}: {}; ", to_pascal_ident(&field.name), self.gen.type_name(&field.ty)));
                }
                params = params.strip_suffix("; ").unwrap().to_string();
                let function_name = format!("{prefix}_create");
                let result_var_name = function_name.clone();
                let func_sig = format!("function {function_name}({params}): {name};");
                self.src.h_helpers(&format!("{func_sig}\n"));
                self.src.c_helpers(&format!("{func_sig}\nbegin\n"));
                for field in r.fields.iter() {
                    self.src.c_helpers(&format!("{result_var_name}.{0} := a{0};\n", to_pascal_ident(&field.name)));
                }
                self.src.c_helpers(&format!("end;\n"));
            }

            TypeDefKind::Tuple(t) => {
                let mut params = String::new();
                for (i, ty) in t.types.iter().enumerate() {
                    params.push_str(&format!("const a{i}: {}; ", self.gen.type_name(&ty)));
                }
                params = params.strip_suffix("; ").unwrap().to_string();
                let function_name = format!("{prefix}_create");
                let result_var_name = function_name.clone();
                let func_sig = format!("function {function_name}({params}): {name};");
                self.src.h_helpers(&format!("{func_sig}\n"));
                self.src.c_helpers(&format!("{func_sig}\nbegin\n"));
                for (i, _ty) in t.types.iter().enumerate() {
                    self.src.c_helpers(&format!("{result_var_name}.f{i} := a{i};\n"));
                }
                self.src.c_helpers(&format!("end;\n"));

                //for (i, ty) in t.types.iter().enumerate() {
                //    self.free(ty, &format!("&ptr->f{i}"));
                //}
            }

            TypeDefKind::List(t) => {
                let t_name = self.gen.type_name(t);
                let function_name = format!("{prefix}_create");
                let result_var_name = function_name.clone();
                let func_sig = format!("function {function_name}(ptr: P{t_name}; len: SizeUInt): {name};");
                self.src.h_helpers(&format!("{func_sig}\n"));
                self.src.c_helpers(&format!(
                    "{func_sig}
                    begin
                      {result_var_name}.ptr := ptr;
                      {result_var_name}.len := len;
                    end;"));
            }

            TypeDefKind::Variant(v) => {
                //self.src.c_helpers("switch ((int32_t) ptr->tag) {\n");
                //for (i, case) in v.cases.iter().enumerate() {
                //    if let Some(ty) = &case.ty {
                //        uwriteln!(self.src.c_helpers, "case {}: {{", i);
                //        let expr = format!("&ptr->val.{}", to_c_ident(&case.name));
                //        self.free(ty, &expr);
                //        self.src.c_helpers("break;\n");
                //        self.src.c_helpers("}\n");
                //    }
                //}
                //self.src.c_helpers("}\n");
            }

            TypeDefKind::Option(t) => {
                //self.src.c_helpers("if (ptr->is_some) {\n");
                //self.free(t, "&ptr->val");
                //self.src.c_helpers("}\n");
            }

            TypeDefKind::Result(r) => {
                //self.src.c_helpers("if (!ptr->is_err) {\n");
                //if let Some(ok) = &r.ok {
                //    self.free(ok, "&ptr->val.ok");
                //}
                //if let Some(err) = &r.err {
                //    self.src.c_helpers("} else {\n");
                //    self.free(err, "&ptr->val.err");
                //}
                //self.src.c_helpers("}\n");
            }
            TypeDefKind::Future(_) => todo!("print_constructor for future"),
            TypeDefKind::Stream(_) => todo!("print_constructor for stream"),
            TypeDefKind::ErrorContext => todo!("print_constructor for error-context"),
            TypeDefKind::Resource => {}
            TypeDefKind::Handle(Handle::Borrow(id) | Handle::Own(id)) => {
                //self.free(&Type::Id(*id), "*ptr");
            }
            TypeDefKind::Unknown => unreachable!(),
        }
        ////self.src.c_helpers.as_mut_string().insert_str(c_helpers_var_section_start, &var_section);
        //if c_helpers_body_start == self.src.c_helpers.len() {
        //    self.src.c_helpers.as_mut_string().truncate(c_helpers_start);
        //    self.src.h_helpers.as_mut_string().truncate(h_helpers_start);
        //    return;
        //}
        //self.src.c_helpers("end;\n");
        ////self.gen.dtor_funcs.insert(id, format!("{prefix}_free"));
    }

    fn define_dtor(&mut self, id: TypeId) {
        let h_helpers_start = self.src.h_helpers.len();
        let c_helpers_start = self.src.c_helpers.len();

        let name = self.gen.type_names[&id].clone();
        let prefix = name.strip_suffix("_t").unwrap();

        self.src
            .h_helpers(&format!("\nprocedure {prefix}_free(ptr: P{name});\n"));
        self.src
            .c_helpers(&format!("\nprocedure {prefix}_free(ptr: P{name});\n"));
        let c_helpers_var_section_start = self.src.c_helpers.len();
        let mut var_section = String::new();
        self.src.c_helpers("begin\n");
        let c_helpers_body_start = self.src.c_helpers.len();
        match &self.resolve.types[id].kind {
            TypeDefKind::Type(t) => self.free(t, "ptr"),

            TypeDefKind::Flags(_) => {}
            TypeDefKind::Enum(_) => {}

            TypeDefKind::Record(r) => {
                for field in r.fields.iter() {
                    self.free(&field.ty, &format!("@(ptr^.{})", to_pascal_ident(&field.name)));
                }
            }

            TypeDefKind::Tuple(t) => {
                for (i, ty) in t.types.iter().enumerate() {
                    self.free(ty, &format!("@(ptr^.f{i})"));
                }
            }

            TypeDefKind::List(t) => {
                self.src.c_helpers("  list_len := ptr^.len;\n");
                uwriteln!(self.src.c_helpers, "  if list_len > 0 then\n  begin");
                let mut t_name = String::new();
                self.gen.push_type_name(t, &mut t_name);
                var_section = format!("var
                  i: SizeUInt;
                  list_len: SizeUInt;
                  list_ptr: P{t_name};\n");
                self.src
                    .c_helpers("list_ptr := ptr^.ptr;\n");
                self.src
                    .c_helpers("for i := 0 to list_len - 1 do\nbegin\n");
                self.free(t, &format!("@list_ptr[i]"));
                self.src.c_helpers("end;\n");
                uwriteln!(self.src.c_helpers, "    FreeMem(list_ptr);");
                uwriteln!(self.src.c_helpers, "  end;");
            }

            TypeDefKind::Variant(v) => {
                self.src.c_helpers("case int32(ptr^.tag) of\n");
                for (i, case) in v.cases.iter().enumerate() {
                    if let Some(ty) = &case.ty {
                        uwriteln!(self.src.c_helpers, "{}:\nbegin\n", i);
                        let expr = format!("@(ptr^.{})", to_pascal_ident(&case.name));
                        self.free(ty, &expr);
                        self.src.c_helpers("end;\n");
                    }
                }
                self.src.c_helpers("end;\n");
            }

            TypeDefKind::Option(t) => {
                self.src.c_helpers("if ptr^.is_some then\nbegin\n");
                self.free(t, "@(ptr^.val)");
                self.src.c_helpers("end;\n");
            }

            TypeDefKind::Result(r) => {
                self.src.c_helpers("if not ptr^.is_err then\nbegin\n");
                if let Some(ok) = &r.ok {
                    self.free(ok, "@(ptr^.ok)");
                }
                if let Some(err) = &r.err {
                    self.src.c_helpers("end else begin\n");
                    self.free(err, "@(ptr^.err)");
                }
                self.src.c_helpers("end;\n");
            }
            TypeDefKind::Future(_) => todo!("print_dtor for future"),
            TypeDefKind::Stream(_) => todo!("print_dtor for stream"),
            TypeDefKind::ErrorContext => todo!("print_dtor for error-context"),
            TypeDefKind::Resource => {}
            TypeDefKind::Handle(Handle::Borrow(id) | Handle::Own(id)) => {
                self.free(&Type::Id(*id), "*ptr");
            }
            TypeDefKind::Unknown => unreachable!(),
        }
        self.src.c_helpers.as_mut_string().insert_str(c_helpers_var_section_start, &var_section);
        if c_helpers_body_start == self.src.c_helpers.len() {
            self.src.c_helpers.as_mut_string().truncate(c_helpers_start);
            self.src.h_helpers.as_mut_string().truncate(h_helpers_start);
            return;
        }
        self.src.c_helpers("end;\n");
        self.gen.dtor_funcs.insert(id, format!("{prefix}_free"));
    }

    fn free(&mut self, ty: &Type, expr: &str) {
        match ty {
            Type::Id(id) => {
                if let Some(dtor) = self.gen.dtor_funcs.get(&id) {
                    self.src.c_helpers(&format!("{dtor}({expr});\n"));
                }
            }
            Type::String => {
                let snake = self.gen.world.to_snake_case();
                self.src
                    .c_helpers(&format!("{snake}_string_free({expr});\n"));
            }
            Type::Bool
            | Type::U8
            | Type::S8
            | Type::U16
            | Type::S16
            | Type::U32
            | Type::S32
            | Type::U64
            | Type::S64
            | Type::F32
            | Type::F64
            | Type::Char => {}
        }
    }

    fn c_func_name(&self, interface_id: Option<&WorldKey>, func: &Function) -> String {
        c_func_name(
            self.in_import,
            self.resolve,
            &self.gen.world,
            interface_id,
            func,
            &self.gen.renamed_interfaces,
        )
    }

    fn import(&mut self, interface_name: Option<&WorldKey>, func: &Function) {
        self.docs(&func.docs, SourceType::HFns);
        let sig = self.resolve.wasm_signature(AbiVariant::GuestImport, func);

        self.src.c_fns("\n");

        // In the private C file, print a function declaration which is the
        // actual wasm import that we'll be calling, and this has the raw wasm
        // signature.
        //uwriteln!(
        //    self.src.c_fns,
        //    "__attribute__((__import_module__(\"{}\"), __import_name__(\"{}\")))",
        //    match interface_name {
        //        Some(name) => self.resolve.name_world_key(name),
        //        None => "$root".to_string(),
        //    },
        //    func.name
        //);
        let name = self.c_func_name(interface_name, func);
        let import_name = self.gen.names.tmp(&format!("__wasm_import_{name}",));
        //self.src.c_fns("extern ");
        match sig.results.len() {
            0 => self.src.c_fns("procedure"),
            1 => self.src.c_fns("function"),
            _ => unimplemented!("multi-value return not supported"),
        }
        self.src.c_fns(" ");
        self.src.c_fns(&import_name);
        self.src.c_fns("(");
        for (i, param) in sig.params.iter().enumerate() {
            if i > 0 {
                self.src.c_fns("; ");
            }
            uwrite!(
                self.src.c_fns,
                "para{}: ",
                i + 1
            );
            self.src.c_fns(wasm_type(*param));
        }
        //if sig.params.len() == 0 {
        //    self.src.c_fns("void");
        //}
        self.src.c_fns(")");
        match sig.results.len() {
            0 => (),
            1 => {
                self.src.c_fns(": ");
                self.src.c_fns(wasm_type(sig.results[0]));
            },
            _ => unimplemented!("multi-value return not supported"),
        }
        self.src.c_fns(";\n");

        // Print the public facing signature into the header, and since that's
        // what we are defining also print it into the C file.
        //self.src.h_fns("extern ");
        let c_sig = self.print_sig(interface_name, func, !self.gen.opts.no_sig_flattening);
        //self.src.h_fns("external;");
        uwriteln!(
            self.src.c_fns,
            "external '{}' name '{}';",
            match interface_name {
                Some(name) => self.resolve.name_world_key(name),
                None => "$root".to_string(),
            },
            func.name
        );
        self.src.c_adapters("\n");
        self.src.c_adapters(&c_sig.sig);
        self.src.c_adapters(";\n");
        let src_var_section_start = self.src.c_adapters.len();
        self.src.c_adapters("begin\n");

        // construct optional adapters from maybe pointers to real optional
        // structs internally
        let mut f = FunctionBindgen::new(self, c_sig, &import_name);
        let mut optional_adapters = String::from("");
        if !f.gen.gen.opts.no_sig_flattening {
            for (i, (_, param)) in f.sig.params.iter().enumerate() {
                let ty = &func.params[i].1;
                if let Type::Id(id) = ty {
                    if let TypeDefKind::Option(_) = &f.gen.resolve.types[*id].kind {
                        let ty = f.gen.gen.type_name(ty);
                        f.local_vars.insert(param, &ty);
                        uwrite!(
                            optional_adapters,
                            "{param}.is_some := maybe_{param} <> nil;"
                        );
                        uwriteln!(
                            optional_adapters,
                            "if maybe_{param} then
                            begin
                              {param}.val := maybe_{param}^;
                            end;",
                        );
                    }
                }
            }
        }

        for (pointer, param) in f.sig.params.iter() {
            f.locals.insert(&param).unwrap();
            if *pointer {
                f.params.push(format!("{}^", param));
            } else {
                f.params.push(param.clone());
            }
        }
        for ptr in f.sig.retptrs.iter() {
            f.locals.insert(ptr).unwrap();
        }
        f.src.push_str(&optional_adapters);
        abi::call(
            f.gen.resolve,
            AbiVariant::GuestImport,
            LiftLower::LowerArgsLiftResults,
            func,
            &mut f,
            false,
        );

        let FunctionBindgen {
            src,
            mut local_vars,
            import_return_pointer_area_size,
            import_return_pointer_area_align,
            ..
        } = f;

        if import_return_pointer_area_size > 0 {
            local_vars.insert("ret_area", &format!("array[0..{}] of byte", import_return_pointer_area_size - 1));
            //var_section.push_str(&format!(
            //    "\
            //        //__attribute__((__aligned__({import_return_pointer_area_align})))
            //        ret_area: array[0..{import_return_pointer_area_size}-1] of byte;
            //    ",
            //));
        }

        self.src.c_adapters(&String::from(src));
        if !local_vars.is_empty() {
            self.src.c_adapters.as_mut_string().insert_str(src_var_section_start, &local_vars.to_string());
        }
        self.src.c_adapters("end;\n");
    }

    fn export(&mut self, func: &Function, interface_name: Option<&WorldKey>) {
        let sig = self.resolve.wasm_signature(AbiVariant::GuestExport, func);

        self.src.c_fns("\n");

        let core_module_name = interface_name.map(|s| self.resolve.name_world_key(s));
        let export_name = func.legacy_core_export_name(core_module_name.as_deref());

        // Print the actual header for this function into the header file, and
        // it's what we'll be calling.
        let h_sig = self.print_sig(interface_name, func, !self.gen.opts.no_sig_flattening);

        // Generate, in the C source file, the raw wasm signature that has the
        // canonical ABI.
        uwriteln!(
            self.src.c_adapters,
            "\n//__attribute__((__export_name__(\"{export_name}\")))"
        );
        let name = self.c_func_name(interface_name, func);
        let import_name = self.gen.names.tmp(&format!("__wasm_export_{name}"));

        let mut f = FunctionBindgen::new(self, h_sig, &import_name);
        match sig.results.len() {
            0 => f.gen.src.c_adapters("procedure"),
            1 => f.gen.src.c_adapters("function"),
            _ => unimplemented!("multi-value return not supported"),
        }
        f.gen.src.c_adapters(" ");
        f.gen.src.c_adapters(&import_name);
        f.gen.src.c_adapters("(");
        for (i, param) in sig.params.iter().enumerate() {
            if i > 0 {
                f.gen.src.c_adapters(", ");
            }
            let name = f.locals.tmp("arg");
            uwrite!(f.gen.src.c_adapters, "{} {}", wasm_type(*param), name);
            f.params.push(name);
        }
        //if sig.params.len() == 0 {
        //    f.gen.src.c_adapters("void");
        //}
        f.gen.src.c_adapters(")");
        match sig.results.len() {
            0 => (),
            1 => {
                f.gen.src.c_adapters(": ");
                f.gen.src.c_adapters(wasm_type(sig.results[0]));
            },
            _ => unimplemented!("multi-value return not supported"),
        }
        f.gen.src.c_adapters(";\nbegin\n");

        // Perform all lifting/lowering and append it to our src.
        abi::call(
            f.gen.resolve,
            AbiVariant::GuestExport,
            LiftLower::LiftArgsLowerResults,
            func,
            &mut f,
            false,
        );
        let FunctionBindgen { src, .. } = f;
        self.src.c_adapters(&src);
        self.src.c_adapters("end;\n");

        if abi::guest_export_needs_post_return(self.resolve, func) {
            uwriteln!(
                self.src.c_fns,
                "__attribute__((__weak__, __export_name__(\"cabi_post_{export_name}\")))"
            );
            uwrite!(self.src.c_fns, "void {import_name}_post_return(");

            let mut params = Vec::new();
            let mut c_sig = CSig {
                name: String::from("INVALID"),
                sig: String::from("INVALID"),
                params: Vec::new(),
                ret: Return::default(),
                retptrs: Vec::new(),
            };
            for (i, result) in sig.results.iter().enumerate() {
                let name = format!("arg{i}");
                uwrite!(self.src.c_fns, "{} {name}", wasm_type(*result));
                c_sig.params.push((false, name.clone()));
                params.push(name);
            }
            self.src.c_fns.push_str(") {\n");

            let mut f = FunctionBindgen::new(self, c_sig, &import_name);
            f.params = params;
            abi::post_return(f.gen.resolve, func, &mut f, false);
            let FunctionBindgen { src, .. } = f;
            self.src.c_fns(&src);
            self.src.c_fns("}\n");
        }
    }

    fn print_sig(
        &mut self,
        interface_name: Option<&WorldKey>,
        func: &Function,
        sig_flattening: bool,
    ) -> CSig {
        let name = self.c_func_name(interface_name, func);
        self.gen.names.insert(&name).expect("duplicate symbols");

        let start = self.src.h_fns.len();
        let mut result_rets = false;
        let mut result_rets_has_ok_type = false;

        let ret = self.classify_ret(func, sig_flattening);
        let (is_function, ret_type_str) = match &ret.scalar {
            None | Some(Scalar::Void) => (false, String::new()),
            Some(Scalar::OptionBool(_id)) => (true, "boolean".to_string()),
            Some(Scalar::ResultBool(ok, _err)) => {
                result_rets = true;
                result_rets_has_ok_type = ok.is_some();
                (true, "boolean".to_string())
            }
            Some(Scalar::Type(ty)) => (true, self.gen.type_name(ty)),
        };
        if is_function {
            self.src.h_fns("function");
        } else {
            self.src.h_fns("procedure");
        }
        self.src.h_fns(" ");
        self.src.h_fns(&name);
        self.src.h_fns("(");
        let mut params = Vec::new();
        for (i, (name, ty)) in func.params.iter().enumerate() {
            if i > 0 {
                self.src.h_fns("; ");
            }
            let pointer = is_arg_by_pointer(self.resolve, ty);
            // optional param pointer sig_flattening
            let optional_type = if let Type::Id(id) = ty {
                if let TypeDefKind::Option(option_ty) = &self.resolve.types[*id].kind {
                    if sig_flattening {
                        Some(option_ty)
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            };
            let (print_ty, print_name) = if sig_flattening {
                if let Some(option_ty) = optional_type {
                    (option_ty, format!("maybe_{}", to_pascal_ident(name)))
                } else {
                    (ty, to_pascal_ident(name))
                }
            } else {
                (ty, to_pascal_ident(name))
            };
            self.src.h_fns(&print_name);
            self.src.h_fns(": ");
            if pointer {
                self.src.h_fns("P");
            }
            self.print_ty(SourceType::HFns, print_ty);
            params.push((optional_type.is_none() && pointer, to_pascal_ident(name)));
        }
        let mut retptrs = Vec::new();
        let single_ret = ret.retptrs.len() == 1;
        for (i, ty) in ret.retptrs.iter().enumerate() {
            if i > 0 || func.params.len() > 0 {
                self.src.h_fns("; ");
            }
            let name: String = if result_rets {
                assert!(i <= 1);
                if i == 0 && result_rets_has_ok_type {
                    "ret".into()
                } else {
                    "err".into()
                }
            } else if single_ret {
                "ret".into()
            } else {
                format!("ret{}", i)
            };
            self.src.h_fns(&name);
            retptrs.push(name);
            self.src.h_fns(": P");
            self.print_ty(SourceType::HFns, ty);
        }
        //if func.params.len() == 0 && ret.retptrs.len() == 0 {
        //    self.src.h_fns("void");
        //}
        self.src.h_fns(")");
        if is_function {
            self.src.h_fns(": ");
            self.src.h_fns(ret_type_str.as_str());
        }

        let sig = self.src.h_fns[start..].to_string();
        self.src.h_fns(";\n");

        CSig {
            sig,
            name,
            params,
            ret,
            retptrs,
        }
    }

    fn classify_ret(&mut self, func: &Function, sig_flattening: bool) -> Return {
        let mut ret = Return::default();
        match &func.result {
            None => ret.scalar = Some(Scalar::Void),
            Some(ty) => {
                ret.return_single(self.resolve, ty, ty, sig_flattening);
            }
        }
        return ret;
    }

    fn print_typedef_target(&mut self, id: TypeId) {
        let name = &self.gen.type_names[&id];
        self.src.h_defs(&name);
        self.src.h_defs(";\n");
    }

    fn start_typedef_struct(&mut self, id: TypeId) {
        let name = &self.gen.type_names[&id];
        uwriteln!(
            self.src.h_defs,
            "type
              PP{0} = ^P{0};
              P{0} = ^{0};
              {0} = record",
            &name
            );
    }

    fn finish_typedef_struct(&mut self, id: TypeId) {
        self.src.h_defs("end;");
    }

    fn owner_namespace(&self, id: TypeId) -> String {
        owner_namespace(
            self.interface,
            self.in_import,
            self.gen.world.clone(),
            self.resolve,
            id,
            &self.gen.renamed_interfaces,
        )
    }

    fn print_ty(&mut self, stype: SourceType, ty: &Type) {
        self.gen
            .push_type_name(ty, self.src.src(stype).as_mut_string());
    }

    fn docs(&mut self, docs: &Docs, stype: SourceType) {
        let docs = match &docs.contents {
            Some(docs) => docs,
            None => return,
        };
        let src = self.src.src(stype);
        for line in docs.trim().lines() {
            src.push_str("// ");
            src.push_str(line);
            src.push_str("\n");
        }
    }

    fn autodrop_enabled(&self) -> bool {
        self.gen.opts.autodrop_borrows == Enabled::Yes
    }

    fn contains_droppable_borrow(&self, ty: &Type) -> bool {
        if let Type::Id(id) = ty {
            match &self.resolve.types[*id].kind {
                TypeDefKind::Handle(h) => match h {
                    // Handles to imported resources will need to be dropped, if the context
                    // they're used in is an export.
                    Handle::Borrow(id) => {
                        !self.in_import
                            && matches!(
                                self.gen.resources[&dealias(self.resolve, *id)].direction,
                                Direction::Import
                            )
                    }

                    Handle::Own(_) => false,
                },

                TypeDefKind::Resource | TypeDefKind::Flags(_) | TypeDefKind::Enum(_) => false,

                TypeDefKind::Record(r) => r
                    .fields
                    .iter()
                    .any(|f| self.contains_droppable_borrow(&f.ty)),

                TypeDefKind::Tuple(t) => {
                    t.types.iter().any(|ty| self.contains_droppable_borrow(ty))
                }

                TypeDefKind::Variant(v) => v.cases.iter().any(|case| {
                    case.ty
                        .as_ref()
                        .map_or(false, |ty| self.contains_droppable_borrow(ty))
                }),

                TypeDefKind::Option(ty) => self.contains_droppable_borrow(ty),

                TypeDefKind::Result(r) => {
                    r.ok.as_ref()
                        .map_or(false, |ty| self.contains_droppable_borrow(ty))
                        || r.err
                            .as_ref()
                            .map_or(false, |ty| self.contains_droppable_borrow(ty))
                }

                TypeDefKind::List(ty) => self.contains_droppable_borrow(ty),

                TypeDefKind::Future(_) | TypeDefKind::Stream(_) | TypeDefKind::ErrorContext => {
                    false
                }

                TypeDefKind::Type(ty) => self.contains_droppable_borrow(ty),

                TypeDefKind::Unknown => false,
            }
        } else {
            false
        }
    }
}

struct DroppableBorrow {
    name: String,
    ty: TypeId,
}

struct PascalVarList {
    defined: HashMap<String, String>,
}

impl PascalVarList {
    fn new() -> PascalVarList {
        PascalVarList {
            defined: Default::default()
        }
    }

    fn is_empty(&self) -> bool {
        self.defined.is_empty()
    }

    pub fn insert(&mut self, name: &str, typ: &str) {
        if self.defined.insert(name.to_string(), typ.to_string()).is_some() {
            panic!("name `{}` already defined", name);
        }
    }
}

impl std::fmt::Display for PascalVarList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "var")?;
        for (name, typ) in &self.defined {
            writeln!(f, "  {}: {};", name, typ)?;
        }
        std::fmt::Result::Ok(())
    }
}

struct FunctionBindgen<'a, 'b> {
    gen: &'a mut InterfaceGenerator<'b>,
    locals: Ns,
    local_vars: PascalVarList,
    src: wit_bindgen_core::Source,
    sig: CSig,
    func_to_call: &'a str,
    block_storage: Vec<wit_bindgen_core::Source>,
    blocks: Vec<(String, Vec<String>)>,
    payloads: Vec<String>,
    params: Vec<String>,
    wasm_return: Option<String>,
    ret_store_cnt: usize,
    import_return_pointer_area_size: usize,
    import_return_pointer_area_align: usize,

    /// Borrows observed during lifting an export, that will need to be dropped when the guest
    /// function exits.
    borrows: Vec<DroppableBorrow>,

    /// Forward declarations for temporary storage of borrow copies.
    borrow_decls: wit_bindgen_core::Source,
}

impl<'a, 'b> FunctionBindgen<'a, 'b> {
    fn new(
        gen: &'a mut InterfaceGenerator<'b>,
        sig: CSig,
        func_to_call: &'a str,
    ) -> FunctionBindgen<'a, 'b> {
        FunctionBindgen {
            gen,
            sig,
            locals: Default::default(),
            local_vars: PascalVarList::new(),
            src: Default::default(),
            func_to_call,
            block_storage: Vec::new(),
            blocks: Vec::new(),
            payloads: Vec::new(),
            params: Vec::new(),
            wasm_return: None,
            ret_store_cnt: 0,
            import_return_pointer_area_size: 0,
            import_return_pointer_area_align: 0,
            borrow_decls: Default::default(),
            borrows: Vec::new(),
        }
    }

    fn store_op(&mut self, op: &str, loc: &str) {
        self.src.push_str(loc);
        self.src.push_str(" := ");
        self.src.push_str(op);
        self.src.push_str(";\n");
    }

    fn load(&mut self, ty: &str, offset: i32, operands: &[String], results: &mut Vec<String>) {
        results.push(format!("P{}({} + {})^", ty, operands[0], offset));
    }

    fn load_ext(&mut self, ty: &str, offset: i32, operands: &[String], results: &mut Vec<String>) {
        self.load(ty, offset, operands, results);
        let result = results.pop().unwrap();
        results.push(format!("int32({})", result));
    }

    fn store(&mut self, ty: &str, offset: i32, operands: &[String]) {
        uwriteln!(
            self.src,
            "P{}({} + {})^ := {};",
            ty,
            operands[1],
            offset,
            operands[0]
        );
    }

    fn store_in_retptr(&mut self, operand: &String) {
        self.store_op(
            operand,
            &format!("{}^", self.sig.retptrs[self.ret_store_cnt]),
        );
        self.ret_store_cnt = self.ret_store_cnt + 1;
    }

    fn empty_return_value(&mut self) {
        // Empty types have no state, so we don't emit stores for them. But we
        // do need to keep track of which return variable we're looking at.
        self.ret_store_cnt = self.ret_store_cnt + 1;
    }

    fn assert_no_droppable_borrows(&self, context: &str, ty: &Type) {
        if !self.gen.in_import
            && self.gen.autodrop_enabled()
            && self.gen.contains_droppable_borrow(ty)
        {
            panic!(
                "Unable to autodrop borrows in `{}` values, please disable autodrop",
                context
            )
        }
    }
}

impl Bindgen for FunctionBindgen<'_, '_> {
    type Operand = String;

    fn sizes(&self) -> &SizeAlign {
        &self.gen.gen.sizes
    }

    fn push_block(&mut self) {
        let prev = mem::take(&mut self.src);
        self.block_storage.push(prev);
    }

    fn finish_block(&mut self, operands: &mut Vec<String>) {
        let to_restore = self.block_storage.pop().unwrap();
        let src = mem::replace(&mut self.src, to_restore);
        self.blocks.push((src.into(), mem::take(operands)));
    }

    fn return_pointer(&mut self, size: usize, align: usize) -> String {
        let ptr = self.locals.tmp("ptr");

        // Use a stack-based return area for imports, because exports need
        // their return area to be live until the post-return call.
        if self.gen.in_import {
            self.import_return_pointer_area_size = self.import_return_pointer_area_size.max(size);
            self.import_return_pointer_area_align =
                self.import_return_pointer_area_align.max(align);
            self.local_vars.insert(&ptr, "Pbyte");
            uwriteln!(self.src, "{} := Pbyte(@ret_area);", ptr);
        } else {
            self.gen.gen.return_pointer_area_size = self.gen.gen.return_pointer_area_size.max(size);
            self.gen.gen.return_pointer_area_align =
                self.gen.gen.return_pointer_area_align.max(align);
            // Declare a statically-allocated return area.
            self.local_vars.insert(&ptr, "Pbyte");
            uwriteln!(self.src, "{} := Pbyte(@STATIC_RET_AREA);", ptr);
        }

        ptr
    }

    fn is_list_canonical(&self, resolve: &Resolve, ty: &Type) -> bool {
        resolve.all_bits_valid(ty)
    }

    fn emit(
        &mut self,
        resolve: &Resolve,
        inst: &Instruction<'_>,
        operands: &mut Vec<String>,
        results: &mut Vec<String>,
    ) {
        match inst {
            Instruction::GetArg { nth } => results.push(self.params[*nth].clone()),
            Instruction::I32Const { val } => results.push(val.to_string()),
            Instruction::ConstZero { tys } => {
                for _ in tys.iter() {
                    results.push("0".to_string());
                }
            }

            // TODO: checked?
            Instruction::U8FromI32 => results.push(format!("byte({})", operands[0])),
            Instruction::S8FromI32 => results.push(format!("int8({})", operands[0])),
            Instruction::U16FromI32 => results.push(format!("uint16({})", operands[0])),
            Instruction::S16FromI32 => results.push(format!("int16({})", operands[0])),
            Instruction::U32FromI32 => results.push(format!("uint32({})", operands[0])),
            Instruction::S32FromI32 | Instruction::S64FromI64 => results.push(operands[0].clone()),
            Instruction::U64FromI64 => results.push(format!("uint64({})", operands[0])),

            Instruction::I32FromU8
            | Instruction::I32FromS8
            | Instruction::I32FromU16
            | Instruction::I32FromS16
            | Instruction::I32FromU32 => {
                results.push(format!("int32({})", operands[0]));
            }
            Instruction::I32FromS32 | Instruction::I64FromS64 => results.push(operands[0].clone()),
            Instruction::I64FromU64 => {
                results.push(format!("int64({})", operands[0]));
            }

            // f32/f64 have the same representation in the import type and in C,
            // so no conversions necessary.
            Instruction::CoreF32FromF32
            | Instruction::CoreF64FromF64
            | Instruction::F32FromCoreF32
            | Instruction::F64FromCoreF64 => {
                results.push(operands[0].clone());
            }

            // TODO: checked
            Instruction::CharFromI32 => {
                results.push(format!("uint32({})", operands[0]));
            }
            Instruction::I32FromChar => {
                results.push(format!("int32({})", operands[0]));
            }

            Instruction::Bitcasts { casts } => {
                for (cast, op) in casts.iter().zip(operands) {
                    let op = self.gen.gen.perform_cast(op, cast);
                    results.push(op);
                }
            }

            Instruction::BoolFromI32 => {
                results.push(format!("(({}) <> 0)", operands[0]));
            }

            Instruction::I32FromBool => {
                results.push(format!("int32(ord({}))", operands[0]));
            }

            Instruction::RecordLower { record, .. } => {
                let op = &operands[0];
                for f in record.fields.iter() {
                    results.push(format!("({}).{}", op, to_pascal_ident(&f.name)));
                }
            }
            Instruction::RecordLift { ty, record, .. } => {
                let name = self.gen.gen.type_name(&Type::Id(*ty));
                let mut result = format!("{}_create(\n", name.strip_suffix("_t").unwrap());
                for (field, op) in record.fields.iter().zip(operands.iter()) {
                    let field_ty = self.gen.gen.type_name(&field.ty);
                    uwriteln!(result, "{}({}),", field_ty, op);
                }
                if result.ends_with(",\n") {
                    result.pop();
                    result.pop();
                }
                result.push_str(")");
                results.push(result);
            }

            Instruction::TupleLower { tuple, .. } => {
                let op = &operands[0];
                for i in 0..tuple.types.len() {
                    results.push(format!("({}).f{}", op, i));
                }
            }
            Instruction::TupleLift { ty, tuple, .. } => {
                let name = self.gen.gen.type_name(&Type::Id(*ty));
                let mut result = format!("{}_create(\n", name.strip_suffix("_t").unwrap());
                for (ty, op) in tuple.types.iter().zip(operands.iter()) {
                    let ty = self.gen.gen.type_name(&ty);
                    uwriteln!(result, "{}({}),", ty, op);
                }
                if result.ends_with(",\n") {
                    result.pop();
                    result.pop();
                }
                result.push_str(")");
                results.push(result);
            }

            Instruction::HandleLower { .. } => {
                let op = &operands[0];
                results.push(format!("({op}).__handle"))
            }

            Instruction::HandleLift { handle, ty, .. } => match handle {
                Handle::Borrow(resource)
                    if matches!(
                        self.gen.gen.resources[&dealias(resolve, *resource)].direction,
                        Direction::Export
                    ) =>
                {
                    // Here we've received a borrow of a resource which we've exported ourselves, so we can treat
                    // it as a raw pointer rather than an opaque handle.
                    let op = &operands[0];
                    let name = self
                        .gen
                        .gen
                        .type_name(&Type::Id(dealias(resolve, *resource)));
                    results.push(format!("(({name}*) {op})"))
                }
                _ => {
                    let op = &operands[0];
                    let name = self.gen.gen.type_name(&Type::Id(*ty));
                    results.push(format!("{name}({op})"));

                    if let Handle::Borrow(id) = handle {
                        if !self.gen.in_import && self.gen.autodrop_enabled() {
                            // Here we've received a borrow of an imported resource, which is the
                            // kind we'll need to drop when the exported function is returning.
                            let ty = dealias(self.gen.resolve, *id);

                            let name = self.locals.tmp("borrow");
                            uwriteln!(self.borrow_decls, "int32_t {name} = 0;");
                            uwriteln!(self.src, "{name} = {op};");

                            self.borrows.push(DroppableBorrow { name, ty });
                        }
                    }
                }
            },

            // TODO: checked
            Instruction::FlagsLower { flags, ty, .. } => match flags_repr(flags) {
                Int::U8 | Int::U16 | Int::U32 => {
                    results.push(operands.pop().unwrap());
                }
                Int::U64 => {
                    let name = self.gen.gen.type_name(&Type::Id(*ty));
                    let tmp = self.locals.tmp("flags");
                    uwriteln!(self.src, "{name} {tmp} = {};", operands[0]);
                    results.push(format!("{tmp} & 0xffffffff"));
                    results.push(format!("({tmp} >> 32) & 0xffffffff"));
                }
            },

            Instruction::FlagsLift { flags, ty, .. } => match flags_repr(flags) {
                Int::U8 | Int::U16 | Int::U32 => {
                    results.push(operands.pop().unwrap());
                }
                Int::U64 => {
                    let name = self.gen.gen.type_name(&Type::Id(*ty));
                    let op0 = &operands[0];
                    let op1 = &operands[1];
                    results.push(format!("(({name}) ({op0})) | ((({name}) ({op1})) << 32)"));
                }
            },

            Instruction::VariantPayloadName => {
                let name = self.locals.tmp("payload");
                results.push(format!("{}^", name));
                self.payloads.push(name);
            }

            Instruction::VariantLower {
                variant,
                results: result_types,
                ..
            } => {
                let blocks = self
                    .blocks
                    .drain(self.blocks.len() - variant.cases.len()..)
                    .collect::<Vec<_>>();
                let payloads = self
                    .payloads
                    .drain(self.payloads.len() - variant.cases.len()..)
                    .collect::<Vec<_>>();

                let mut variant_results = Vec::with_capacity(result_types.len());
                for ty in result_types.iter() {
                    let name = self.locals.tmp("variant");
                    results.push(name.clone());
                    self.local_vars.insert(&name, wasm_type(*ty));
                    variant_results.push(name);
                }

                let expr_to_match = format!("({}).tag", operands[0]);

                uwriteln!(self.src, "case int32({}) of", expr_to_match);
                for (i, ((case, (block, block_results)), payload)) in
                    variant.cases.iter().zip(blocks).zip(payloads).enumerate()
                {
                    uwriteln!(self.src, "{}:\nbegin", i);
                    if let Some(ty) = case.ty.as_ref() {
                        let ty = self.gen.gen.type_name(ty);
                        self.local_vars.insert(&payload, &format!("P{ty}"));
                        uwrite!(
                            self.src,
                            "{} := @({})",
                            payload,
                            operands[0],
                        );
                        self.src.push_str(".");
                        self.src.push_str(&to_pascal_ident(&case.name));
                        self.src.push_str(";\n");
                    }
                    self.src.push_str(&block);

                    for (name, result) in variant_results.iter().zip(&block_results) {
                        uwriteln!(self.src, "{} := {};", name, result);
                    }
                    self.src.push_str("end;\n");
                }
                self.src.push_str("end;\n");
            }

            Instruction::VariantLift { variant, ty, .. } => {
                let blocks = self
                    .blocks
                    .drain(self.blocks.len() - variant.cases.len()..)
                    .collect::<Vec<_>>();

                let ty = self.gen.gen.type_name(&Type::Id(*ty));
                let result = self.locals.tmp("variant");
                self.local_vars.insert(&result, &ty);
                uwriteln!(self.src, "{}.tag := {};", result, operands[0]);
                uwriteln!(self.src, "case int32({}.tag) of", result);
                for (i, (case, (block, block_results))) in
                    variant.cases.iter().zip(blocks).enumerate()
                {
                    uwriteln!(self.src, "{}:\nbegin", i);
                    self.src.push_str(&block);
                    assert!(block_results.len() == (case.ty.is_some() as usize));

                    if let Some(_) = case.ty.as_ref() {
                        let mut dst = format!("{}", result);
                        dst.push_str(".");
                        dst.push_str(&to_pascal_ident(&case.name));
                        self.store_op(&block_results[0], &dst);
                    }
                    self.src.push_str("end;\n");
                }
                self.src.push_str("end;\n");
                results.push(result);
            }

            Instruction::OptionLower {
                results: result_types,
                payload,
                ..
            } => {
                let (mut some, some_results) = self.blocks.pop().unwrap();
                let (mut none, none_results) = self.blocks.pop().unwrap();
                let some_payload = self.payloads.pop().unwrap();
                let _none_payload = self.payloads.pop().unwrap();

                for (i, ty) in result_types.iter().enumerate() {
                    let name = self.locals.tmp("option");
                    results.push(name.clone());
                    self.src.push_str(wasm_type(*ty));
                    self.src.push_str(" ");
                    self.src.push_str(&name);
                    self.src.push_str(";\n");
                    let some_result = &some_results[i];
                    uwriteln!(some, "{name} = {some_result};");
                    let none_result = &none_results[i];
                    uwriteln!(none, "{name} = {none_result};");
                }

                let op0 = &operands[0];
                let ty = self.gen.gen.type_name(payload);
                let bind_some = format!("const {ty} *{some_payload} = &({op0}).val;");

                uwrite!(
                    self.src,
                    "\
                    if (({op0}).is_some) {{
                        {bind_some}
                        {some}}} else {{
                        {none}}}
                    "
                );
            }

            Instruction::OptionLift { ty, .. } => {
                let (mut some, some_results) = self.blocks.pop().unwrap();
                let (mut none, none_results) = self.blocks.pop().unwrap();
                assert!(none_results.len() == 0);
                assert!(some_results.len() == 1);
                let some_result = &some_results[0];

                let ty = self.gen.gen.type_name(&Type::Id(*ty));
                let result = self.locals.tmp("option");
                self.local_vars.insert(&result, &ty);
                let op0 = &operands[0];
                let set_some = format!("{result}.val := {some_result};\n");
                if none.len() > 0 {
                    none.push('\n');
                }
                if some.len() > 0 {
                    some.push('\n');
                }
                uwrite!(
                    self.src,
                    "case {op0} of
                        0:
                        begin
                            {result}.is_some := false;
                            {none}\
                        end;
                        1:
                        begin
                            {result}.is_some := true;
                            {some}\
                            {set_some}\
                        end;
                    end;\n"
                );
                results.push(result);
            }

            Instruction::ResultLower {
                results: result_types,
                result,
                ..
            } => {
                let (mut err, err_results) = self.blocks.pop().unwrap();
                let (mut ok, ok_results) = self.blocks.pop().unwrap();
                let err_payload = self.payloads.pop().unwrap();
                let ok_payload = self.payloads.pop().unwrap();

                for (i, ty) in result_types.iter().enumerate() {
                    let name = self.locals.tmp("result");
                    results.push(name.clone());
                    self.local_vars.insert(&name, wasm_type(*ty));
                    let ok_result = &ok_results[i];
                    uwriteln!(ok, "{name} := {ok_result};");
                    let err_result = &err_results[i];
                    uwriteln!(err, "{name} := {err_result};");
                }

                let op0 = &operands[0];
                let bind_ok = if let Some(ok) = result.ok.as_ref() {
                    let ok_ty = self.gen.gen.type_name(ok);
                    format!("const {ok_ty} *{ok_payload} = &({op0}).val.ok;")
                } else {
                    String::new()
                };
                let bind_err = if let Some(err) = result.err.as_ref() {
                    let err_ty = self.gen.gen.type_name(err);
                    format!("const {err_ty} *{err_payload} = &({op0}).val.err;")
                } else {
                    String::new()
                };
                uwrite!(
                    self.src,
                    "\
                    if ({op0}).is_err then
                    begin
                      {bind_err}\
                      {err}\
                    end
                    else
                    begin
                      {bind_ok}\
                      {ok}\
                    end;
                    "
                );
            }

            Instruction::ResultLift { result, ty, .. } => {
                let (mut err, err_results) = self.blocks.pop().unwrap();
                assert!(err_results.len() == (result.err.is_some() as usize));
                let (mut ok, ok_results) = self.blocks.pop().unwrap();
                assert!(ok_results.len() == (result.ok.is_some() as usize));

                if err.len() > 0 {
                    err.push_str("\n");
                }
                if ok.len() > 0 {
                    ok.push_str("\n");
                }

                let result_tmp = self.locals.tmp("result");
                let set_ok = if let Some(_) = result.ok.as_ref() {
                    let ok_result = &ok_results[0];
                    format!("{result_tmp}.ok := {ok_result};\n")
                } else {
                    String::new()
                };
                let set_err = if let Some(_) = result.err.as_ref() {
                    let err_result = &err_results[0];
                    format!("{result_tmp}.err := {err_result};\n")
                } else {
                    String::new()
                };

                let ty = self.gen.gen.type_name(&Type::Id(*ty));
                self.local_vars.insert(&result_tmp, &ty);
                let op0 = &operands[0];
                uwriteln!(
                    self.src,
                    "case {op0} of
                        0:
                        begin
                            {result_tmp}.is_err := false;
                            {ok}\
                            {set_ok}\
                        end;
                        1:
                        begin
                            {result_tmp}.is_err := true;
                            {err}\
                            {set_err}\
                        end;
                    end;"
                );
                results.push(result_tmp);
            }

            Instruction::EnumLower { .. } => results.push(format!("int32({})", operands[0])),
            Instruction::EnumLift { .. } => results.push(operands.pop().unwrap()),

            Instruction::ListCanonLower { .. } | Instruction::StringLower { .. } => {
                results.push(format!("Pbyte(({}).ptr)", operands[0]));
                results.push(format!("({}).len", operands[0]));
            }
            Instruction::ListCanonLift { element, ty, .. } => {
                self.assert_no_droppable_borrows("list", &Type::Id(*ty));

                let list_name = self.gen.gen.type_name(&Type::Id(*ty));
                let elem_name = self.gen.gen.type_name(element);
                results.push(format!(
                    "{}_create(P{}({}), {})",
                    list_name.strip_suffix("_t").unwrap(), elem_name, operands[0], operands[1]
                ));
            }
            Instruction::StringLift { .. } => {
                let list_name = self.gen.gen.type_name(&Type::String);
                results.push(format!(
                    "{}_create(P{}({}), {})",
                    list_name.strip_suffix("_t").unwrap(),
                    self.gen.gen.char_type(),
                    operands[0],
                    operands[1]
                ));
            }

            Instruction::ListLower { .. } => {
                let _body = self.blocks.pop().unwrap();
                results.push(format!("Pbyte(({}).ptr)", operands[0]));
                results.push(format!("({}).len", operands[0]));
            }

            Instruction::ListLift { element, ty, .. } => {
                self.assert_no_droppable_borrows("list", &Type::Id(*ty));

                let _body = self.blocks.pop().unwrap();
                let list_name = self.gen.gen.type_name(&Type::Id(*ty));
                let elem_name = self.gen.gen.type_name(element);
                results.push(format!(
                    "{}_create( P{}({}), {} )",
                    list_name.strip_suffix("_t").unwrap(), elem_name, operands[0], operands[1]
                ));
            }
            Instruction::IterElem { .. } => results.push("e".to_string()),
            Instruction::IterBasePointer => results.push("base".to_string()),

            Instruction::CallWasm { sig, .. } => {
                match sig.results.len() {
                    0 => {}
                    1 => {
                        let ret = self.locals.tmp("ret");
                        self.local_vars.insert(&ret, wasm_type(sig.results[0]));
                        self.wasm_return = Some(ret.clone());
                        uwrite!(self.src, " {} := ", ret);
                        results.push(ret);
                    }
                    _ => unimplemented!(),
                }
                self.src.push_str(self.func_to_call);
                self.src.push_str("(");
                for (i, op) in operands.iter().enumerate() {
                    if i > 0 {
                        self.src.push_str(", ");
                    }
                    self.src.push_str(op);
                }
                self.src.push_str(");\n");
            }

            Instruction::CallInterface { func, .. } => {
                let mut args = String::new();
                for (i, (op, (byref, _))) in operands.iter().zip(&self.sig.params).enumerate() {
                    if i > 0 {
                        args.push_str(", ");
                    }
                    let ty = &func.params[i].1;
                    if *byref {
                        let name = self.locals.tmp("arg");
                        let ty = self.gen.gen.type_name(ty);
                        uwriteln!(self.src, "{} {} = {};", ty, name, op);
                        args.push_str("&");
                        args.push_str(&name);
                    } else {
                        if !self.gen.in_import {
                            if let Type::Id(id) = ty {
                                if let TypeDefKind::Option(_) = &self.gen.resolve.types[*id].kind {
                                    uwrite!(args, "{op}.is_some ? &({op}.val) : NULL");
                                    continue;
                                }
                            }
                        }
                        args.push_str(op);
                    }
                }
                match &self.sig.ret.scalar {
                    None => {
                        let mut retptrs = Vec::new();
                        for ty in self.sig.ret.retptrs.iter() {
                            let name = self.locals.tmp("ret");
                            let ty = self.gen.gen.type_name(ty);
                            uwriteln!(self.src, "{} {};", ty, name);
                            if args.len() > 0 {
                                args.push_str(", ");
                            }
                            args.push_str("&");
                            args.push_str(&name);
                            retptrs.push(name);
                        }
                        uwriteln!(self.src, "{}({});", self.sig.name, args);
                        results.extend(retptrs);
                    }
                    Some(Scalar::Void) => {
                        uwriteln!(self.src, "{}({});", self.sig.name, args);
                    }
                    Some(Scalar::Type(_)) => {
                        let ret = self.locals.tmp("ret");
                        let ty = func.result.unwrap();
                        let ty = self.gen.gen.type_name(&ty);
                        uwriteln!(self.src, "{} {} = {}({});", ty, ret, self.sig.name, args);
                        results.push(ret);
                    }
                    Some(Scalar::OptionBool(ty)) => {
                        let ret = self.locals.tmp("ret");
                        let val = self.locals.tmp("val");
                        if args.len() > 0 {
                            args.push_str(", ");
                        }
                        args.push_str("&");
                        args.push_str(&val);
                        let payload_ty = self.gen.gen.type_name(ty);
                        uwriteln!(self.src, "{} {};", payload_ty, val);
                        uwriteln!(self.src, "bool {} = {}({});", ret, self.sig.name, args);
                        let ty = func.result.unwrap();
                        let option_ty = self.gen.gen.type_name(&ty);
                        let option_ret = self.locals.tmp("ret");
                        uwrite!(
                            self.src,
                            "
                                {option_ty} {option_ret};
                                {option_ret}.is_some = {ret};
                                {option_ret}.val = {val};
                            ",
                        );
                        results.push(option_ret);
                    }
                    Some(Scalar::ResultBool(ok, err)) => {
                        let ty = &func.result.unwrap();
                        let result_ty = self.gen.gen.type_name(ty);
                        let ret = self.locals.tmp("ret");
                        let mut ret_iter = self.sig.ret.retptrs.iter();
                        uwriteln!(self.src, "{result_ty} {ret};");
                        let ok_name = if ok.is_some() {
                            if let Some(ty) = ret_iter.next() {
                                let val = self.locals.tmp("ok");
                                if args.len() > 0 {
                                    uwrite!(args, ", ");
                                }
                                uwrite!(args, "&{val}");
                                let ty = self.gen.gen.type_name(ty);
                                uwriteln!(self.src, "{} {};", ty, val);
                                Some(val)
                            } else {
                                None
                            }
                        } else {
                            None
                        };
                        let err_name = if let Some(ty) = ret_iter.next() {
                            let val = self.locals.tmp("err");
                            if args.len() > 0 {
                                uwrite!(args, ", ")
                            }
                            uwrite!(args, "&{val}");
                            let ty = self.gen.gen.type_name(ty);
                            uwriteln!(self.src, "{} {};", ty, val);
                            Some(val)
                        } else {
                            None
                        };
                        assert!(ret_iter.next().is_none());
                        uwrite!(self.src, "");
                        uwriteln!(self.src, "{ret}.is_err = !{}({args});", self.sig.name);
                        if err.is_some() {
                            if let Some(err_name) = err_name {
                                uwriteln!(
                                    self.src,
                                    "if ({ret}.is_err) {{
                                        {ret}.val.err = {err_name};
                                    }}",
                                );
                            }
                        }
                        if ok.is_some() {
                            if let Some(ok_name) = ok_name {
                                uwriteln!(
                                    self.src,
                                    "if (!{ret}.is_err) {{
                                        {ret}.val.ok = {ok_name};
                                    }}"
                                );
                            } else {
                                uwrite!(self.src, "\n");
                            }
                        }
                        results.push(ret);
                    }
                }
            }
            Instruction::Return { .. } if self.gen.in_import => match self.sig.ret.scalar {
                None => {
                    for op in operands.iter() {
                        self.store_in_retptr(op);
                    }
                }
                Some(Scalar::Void) => {
                    assert!(operands.is_empty());
                }
                Some(Scalar::Type(_)) => {
                    assert_eq!(operands.len(), 1);
                    self.src.push_str("exit(");
                    self.src.push_str(&operands[0]);
                    self.src.push_str(");\n");
                }
                Some(Scalar::OptionBool(_)) => {
                    assert_eq!(operands.len(), 1);
                    let variant = &operands[0];
                    self.store_in_retptr(&format!("{}.val", variant));
                    self.src.push_str("exit(");
                    self.src.push_str(&variant);
                    self.src.push_str(".is_some);\n");
                }
                Some(Scalar::ResultBool(ok, err)) => {
                    assert_eq!(operands.len(), 1);
                    let variant = &operands[0];
                    assert!(self.sig.retptrs.len() <= 2);
                    uwriteln!(self.src, "if not {}.is_err then\nbegin", variant);
                    if ok.is_some() {
                        if ok.is_some() {
                            self.store_in_retptr(&format!("{}.ok", variant));
                        } else {
                            self.empty_return_value();
                        }
                    }
                    uwriteln!(
                        self.src,
                        "   exit(true);
                            end
                            else
                            begin"
                    );
                    if err.is_some() {
                        if err.is_some() {
                            self.store_in_retptr(&format!("{}.err", variant));
                        } else {
                            self.empty_return_value();
                        }
                    }
                    uwriteln!(
                        self.src,
                        "   exit(false);
                            end;"
                    );
                    assert_eq!(self.ret_store_cnt, self.sig.retptrs.len());
                }
            },
            Instruction::Return { amt, .. } => {
                // Emit all temporary borrow decls
                let src = std::mem::replace(&mut self.src, std::mem::take(&mut self.borrow_decls));
                self.src.append_src(&src);

                for DroppableBorrow { name, ty } in self.borrows.iter() {
                    let drop_fn = self.gen.gen.resources[ty].drop_fn.as_str();
                    uwriteln!(self.src, "if ({name} != 0) {{");
                    uwriteln!(self.src, "  {drop_fn}({name});");
                    uwriteln!(self.src, "}}");
                }

                assert!(*amt <= 1);
                if *amt == 1 {
                    uwriteln!(self.src, "return {};", operands[0]);
                }
            }

            Instruction::I32Load { offset } => self.load("int32", *offset, operands, results),
            Instruction::I64Load { offset } => self.load("int64", *offset, operands, results),
            Instruction::F32Load { offset } => self.load("single", *offset, operands, results),
            Instruction::F64Load { offset } => self.load("double", *offset, operands, results),
            Instruction::PointerLoad { offset } => {
                self.load("Pbyte", *offset, operands, results)
            }
            Instruction::LengthLoad { offset } => self.load("SizeUInt", *offset, operands, results),
            Instruction::I32Store { offset } => self.store("int32", *offset, operands),
            Instruction::I64Store { offset } => self.store("int64", *offset, operands),
            Instruction::F32Store { offset } => self.store("single", *offset, operands),
            Instruction::F64Store { offset } => self.store("double", *offset, operands),
            Instruction::I32Store8 { offset } => self.store("int8", *offset, operands),
            Instruction::I32Store16 { offset } => self.store("int16", *offset, operands),
            Instruction::PointerStore { offset } => self.store("Puint8_t", *offset, operands),
            Instruction::LengthStore { offset } => self.store("SizeUInt", *offset, operands),

            Instruction::I32Load8U { offset } => {
                self.load_ext("byte", *offset, operands, results)
            }
            Instruction::I32Load8S { offset } => {
                self.load_ext("int8", *offset, operands, results)
            }
            Instruction::I32Load16U { offset } => {
                self.load_ext("uint16", *offset, operands, results)
            }
            Instruction::I32Load16S { offset } => {
                self.load_ext("int16", *offset, operands, results)
            }

            Instruction::GuestDeallocate { .. } => {
                uwriteln!(self.src, "free({});", operands[0]);
            }
            Instruction::GuestDeallocateString => {
                uwriteln!(self.src, "if (({}) > 0) {{", operands[1]);
                uwriteln!(self.src, "free({});", operands[0]);
                uwriteln!(self.src, "}}");
            }
            Instruction::GuestDeallocateVariant { blocks } => {
                let blocks = self
                    .blocks
                    .drain(self.blocks.len() - blocks..)
                    .collect::<Vec<_>>();

                uwriteln!(self.src, "{{5}}switch ((int32_t) {}) {{", operands[0]);
                for (i, (block, results)) in blocks.into_iter().enumerate() {
                    assert!(results.is_empty());
                    uwriteln!(self.src, "case {}: {{", i);
                    self.src.push_str(&block);
                    self.src.push_str("break;\n}\n");
                }
                self.src.push_str("}\n");
            }
            Instruction::GuestDeallocateList { element } => {
                let (body, results) = self.blocks.pop().unwrap();
                assert!(results.is_empty());
                let len = self.locals.tmp("len");
                uwriteln!(self.src, "size_t {len} = {};", operands[1]);
                uwriteln!(self.src, "if ({len} > 0) {{");
                let ptr = self.locals.tmp("ptr");
                uwriteln!(self.src, "uint8_t *{ptr} = {};", operands[0]);
                let i = self.locals.tmp("i");
                uwriteln!(self.src, "for (size_t {i} = 0; {i} < {len}; {i}++) {{");
                let size = self.gen.gen.sizes.size(element).size_wasm32();
                uwriteln!(self.src, "uint8_t *base = {ptr} + {i} * {size};");
                uwriteln!(self.src, "(void) base;");
                uwrite!(self.src, "{body}");
                uwriteln!(self.src, "}}");
                uwriteln!(self.src, "free({ptr});");
                uwriteln!(self.src, "}}");
            }

            Instruction::Flush { amt } => {
                results.extend(operands.iter().take(*amt).map(|v| v.clone()));
            }

            i => unimplemented!("{:?}", i),
        }
    }
}

#[derive(Default, Clone, Copy)]
enum SourceType {
    #[default]
    HDefs,
    HFns,
    // HHelpers,
    // CDefs,
    // CFns,
    // CHelpers,
    // CAdapters,
}

#[derive(Default)]
struct Source {
    h_defs: wit_bindgen_core::Source,
    h_fns: wit_bindgen_core::Source,
    h_helpers: wit_bindgen_core::Source,
    c_defs: wit_bindgen_core::Source,
    c_fns: wit_bindgen_core::Source,
    c_helpers: wit_bindgen_core::Source,
    c_adapters: wit_bindgen_core::Source,
}

impl Source {
    fn src(&mut self, stype: SourceType) -> &mut wit_bindgen_core::Source {
        match stype {
            SourceType::HDefs => &mut self.h_defs,
            SourceType::HFns => &mut self.h_fns,
        }
    }
    fn append(&mut self, append_src: &Source) {
        self.h_defs.push_str(&append_src.h_defs);
        self.h_fns.push_str(&append_src.h_fns);
        self.h_helpers.push_str(&append_src.h_helpers);
        self.c_defs.push_str(&append_src.c_defs);
        self.c_fns.push_str(&append_src.c_fns);
        self.c_helpers.push_str(&append_src.c_helpers);
        self.c_adapters.push_str(&append_src.c_adapters);
    }
    fn h_defs(&mut self, s: &str) {
        self.h_defs.push_str(s);
    }
    fn h_fns(&mut self, s: &str) {
        self.h_fns.push_str(s);
    }
    fn h_helpers(&mut self, s: &str) {
        self.h_helpers.push_str(s);
    }
    fn c_fns(&mut self, s: &str) {
        self.c_fns.push_str(s);
    }
    fn c_helpers(&mut self, s: &str) {
        self.c_helpers.push_str(s);
    }
    fn c_adapters(&mut self, s: &str) {
        self.c_adapters.push_str(s);
    }
}

fn wasm_type(ty: WasmType) -> &'static str {
    match ty {
        WasmType::I32 => "int32",
        WasmType::I64 => "int64",
        WasmType::F32 => "single",
        WasmType::F64 => "double",
        WasmType::Pointer => "Pbyte",
        WasmType::PointerOrI64 => "int64",
        WasmType::Length => "SizeUInt",
    }
}

pub fn int_repr(ty: Int) -> &'static str {
    match ty {
        Int::U8 => "byte",
        Int::U16 => "uint16",
        Int::U32 => "uint32",
        Int::U64 => "uint64",
    }
}

pub fn flags_repr(f: &Flags) -> Int {
    match f.repr() {
        FlagsRepr::U8 => Int::U8,
        FlagsRepr::U16 => Int::U16,
        FlagsRepr::U32(1) => Int::U32,
        FlagsRepr::U32(2) => Int::U64,
        repr => panic!("unimplemented flags {:?}", repr),
    }
}

pub fn is_arg_by_pointer(resolve: &Resolve, ty: &Type) -> bool {
    match ty {
        Type::Id(id) => match resolve.types[*id].kind {
            TypeDefKind::Type(t) => is_arg_by_pointer(resolve, &t),
            TypeDefKind::Variant(_) => true,
            TypeDefKind::Option(_) => true,
            TypeDefKind::Result(_) => true,
            TypeDefKind::Enum(_) => false,
            TypeDefKind::Flags(_) => false,
            TypeDefKind::Handle(_) => false,
            TypeDefKind::Tuple(_) | TypeDefKind::Record(_) | TypeDefKind::List(_) => true,
            TypeDefKind::Future(_) => todo!("is_arg_by_pointer for future"),
            TypeDefKind::Stream(_) => todo!("is_arg_by_pointer for stream"),
            TypeDefKind::ErrorContext => todo!("is_arg_by_pointer for error-context"),
            TypeDefKind::Resource => todo!("is_arg_by_pointer for resource"),
            TypeDefKind::Unknown => unreachable!(),
        },
        Type::String => true,
        _ => false,
    }
}

pub fn to_pascal_ident(name: &str) -> String {
    match name {
        // Escape Pascal keywords.
        "and" => "and_".into(),
        "array" => "array_".into(),
        "as" => "as_".into(),
        "asm" => "asm_".into(),
        "begin" => "begin_".into(),
        "bitpacked" => "bitpacked_".into(),
        "case" => "case_".into(),
        "class" => "class_".into(),
        "const" => "const_".into(),
        "constref" => "constref_".into(),
        "constructor" => "constructor_".into(),
        "destructor" => "destructor_".into(),
        "div" => "div_".into(),
        "do" => "do_".into(),
        "downto" => "downto_".into(),
        "else" => "else_".into(),
        "end" => "end_".into(),
        "except" => "except_".into(),
        "exports" => "exports_".into(),
        "file" => "file_".into(),
        "finalization" => "finalization_".into(),
        "finally" => "finally_".into(),
        "for" => "for_".into(),
        "function" => "function_".into(),
        "goto" => "goto_".into(),
        "if" => "if_".into(),
        "implementation" => "implementation_".into(),
        "in" => "in_".into(),
        "inherited" => "inherited_".into(),
        "initialization" => "initialization_".into(),
        "interface" => "interface_".into(),
        "is" => "is_".into(),
        "label" => "label_".into(),
        "library" => "library_".into(),
        "mod" => "mod_".into(),
        "nil" => "nil_".into(),
        "not" => "not_".into(),
        "object" => "object_".into(),
        "of" => "of_".into(),
        "operator" => "operator_".into(),
        "or" => "or_".into(),
        "otherwise" => "otherwise_".into(),
        "out" => "out_".into(),
        "packed" => "packed_".into(),
        "procedure" => "procedure_".into(),
        "program" => "program_".into(),
        "property" => "property_".into(),
        "raise" => "raise_".into(),
        "record" => "record_".into(),
        "repeat" => "repeat_".into(),
        "resourcestring" => "resourcestring_".into(),
        "result" => "result_".into(),
        "set" => "set_".into(),
        "shl" => "shl_".into(),
        "shr" => "shr_".into(),
        "specialize" => "specialize_".into(),
        "string" => "string_".into(),
        "then" => "then_".into(),
        "threadvar" => "threadvar_".into(),
        "to" => "to_".into(),
        "try" => "try_".into(),
        "type" => "type_".into(),
        "unit" => "unit_".into(),
        "until" => "until_".into(),
        "uses" => "uses_".into(),
        "var" => "var_".into(),
        "while" => "while_".into(),
        "with" => "with_".into(),
        "xor" => "xor_".into(),
        // ret and err needs to be escaped because they are used as
        //  variable names for option and result flattening.
        "ret" => "ret_".into(),
        "err" => "err_".into(),
        s => s.to_snake_case(),
    }
}
