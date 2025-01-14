//! # build.rs for s3d
//!
//! This is the cargo build script which is called during build.
//! We use it to generate code that for the S3 protocol.
//!
//! It reads the smithy model `models/s3.json` as input,
//! and writes the code out to `$OUT_DIR/`,
//! which is included! in the src/build_gen.rs file.
//!
//! The S3 protocol is defined in a Smithy JSON AST model - see:
//! - https://github.com/awslabs/smithy-rs/blob/main/aws/sdk/aws-models/s3.json
//! - https://awslabs.github.io/smithy/1.0/spec/index.html
//! - https://awslabs.github.io/smithy/1.0/spec/core/json-ast.html

use proc_macro2::{Ident, TokenStream};
use quote::{format_ident, quote};
use serde_json::{Map, Value};
use std::{
    collections::HashMap,
    env,
    fs::File,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    // process::{Child, ChildStdin, Command, Stdio},
};

fn main() {
    let out_dir = env::var("OUT_DIR").unwrap();
    let out_path = Path::new(out_dir.as_str());
    let model_path = Path::new("smithy-rs/aws/sdk/aws-models/s3.json");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", model_path.display());
    let model_json: Value = serde_json::from_reader(File::open(model_path).unwrap()).unwrap();
    let model = SmithyModel::from_json(&model_json);
    Generator::new(&model, &out_path).generate();
}

pub type SmithyShapeSet<'a> = HashMap<String, Option<&'a SmithyShape>>;

/// Generator is the main generator class.
/// It keeps the state of the generation process.
pub struct Generator<'a> {
    pub model: &'a SmithyModel,
    pub writer: Option<CodeWriter>,
    pub out_path: PathBuf,
    pub client_crate: String,
    pub server_crate: String,
    pub ops: SmithyShapeSet<'a>,
    pub inputs: SmithyShapeSet<'a>,
    pub outputs: SmithyShapeSet<'a>,
    pub models: SmithyShapeSet<'a>,
    pub errors: SmithyShapeSet<'a>,
}

impl<'a> Generator<'a> {
    pub fn new(model: &'a SmithyModel, out_path: &Path) -> Self {
        let mut ops = HashMap::new();
        let mut inputs = HashMap::new();
        let mut outputs = HashMap::new();
        let mut models = HashMap::new();
        let errors = HashMap::new();

        for op in model.iter_shapes_by_type(SmithyType::Operation) {
            if op.name == "SelectObjectContent" {
                continue;
            }
            ops.insert(op.name.to_owned(), Some(op));

            let input_shape = op.members.get("input").map(|m| model.get_shape_of(m));
            let output_shape = op.members.get("output").map(|m| model.get_shape_of(m));
            inputs.insert(format!("{}Input", op.name), input_shape);
            outputs.insert(format!("{}Output", op.name), output_shape);

            // collect all the shapes that get their own type in the generated model package
            fn collect_models<'a>(
                model: &'a SmithyModel,
                s: &'a SmithyShape,
                models: &mut SmithyShapeSet<'a>,
            ) {
                for m in s.members.values() {
                    let shape = model.get_shape_of(m);
                    if shape.typ != SmithyType::Structure
                        && shape.typ != SmithyType::Union
                        && !shape.has_trait(SM_ENUM)
                    {
                        continue;
                    }
                    if models.contains_key(&shape.name) {
                        continue;
                    }
                    models.insert(shape.name.to_owned(), Some(shape));
                    collect_models(model, shape, models);
                }
            }
            if let Some(s) = input_shape {
                collect_models(model, s, &mut models);
            }
            if let Some(s) = output_shape {
                collect_models(model, s, &mut models);
            }

            // TODO: handle errors
            // if op.members.contains_key("errors???") {}
        }

        Self {
            model,
            writer: None,
            out_path: out_path.to_path_buf(),
            client_crate: String::from("aws_sdk_s3"),
            server_crate: String::from("s3d_smithy_codegen_server_s3"),
            ops,
            inputs,
            outputs,
            models,
            errors,
        }
    }

    pub fn generate(&mut self) {
        self.set_output_file("s3_ops.rs");
        self.gen_ops_enum();
        self.set_output_file("s3_conv.rs");
        self.gen_converters();
        self.close_output_file();
    }

    /// Generate the basic enum of operation kinds + macros to quickly generate code for each operation.
    /// The enum is flat - meaning it defines no attached state to any of the operations.
    /// It might be interesting to consider a more complex enum if needed by the daemon,
    /// or perhaps that would instead go to it's own enum, with auto-generated-mapping to this one.
    pub fn gen_ops_enum(&mut self) {
        let ops_names: Vec<Ident> = self.ops.values().map(|op| op.unwrap().ident()).collect();

        self.write_code(quote! {

            #[derive(Debug, PartialEq, Eq, Clone, Copy)]
            pub enum S3Ops {
                #(#ops_names),*
            }

            /// This macro calls a provided $macro for each S3 operation to generate code per op.
            macro_rules! generate_code_for_each_s3_op {
                ($macro:ident) => {
                    #( $macro!(#ops_names); )*
                }
            }

            /// This macro matches a variable of S3Ops type and expands the provided $macro
            /// for each S3 operation to generate code handler per op.
            macro_rules! generate_match_for_each_s3_op {
                ($macro:ident, $op:expr) => {
                    match ($op) {
                        #( S3Ops::#ops_names => $macro!(#ops_names), )*
                    }
                }
            }

            pub(crate) use generate_code_for_each_s3_op;
            pub(crate) use generate_match_for_each_s3_op;

        });
    }

    /// see https://github.com/awslabs/smithy-rs/issues/1099
    pub fn gen_converters(&mut self) {
        let client_crate = format_ident!("{}", self.client_crate);
        let server_crate = format_ident!("{}", self.server_crate);

        for op in self.model.iter_shapes_by_type(SmithyType::Operation) {
            if op.name == "SelectObjectContent" {
                continue;
            }
            {
                let input_id = format_ident!("{}Input", op.name);
                let conv_to_client_input =
                    format_ident!("conv_to_client_{}", snake(&input_id.to_string()));
                let conv_to_client_input_gen =
                    if let Some(mut input_member) = op.members.get("input").cloned() {
                        input_member.name = format!("input/{}", input_id);
                        self.gen_conv_to_client(&input_member, quote! { input  })
                    } else {
                        quote! { #client_crate::input::#input_id::builder().build().unwrap() }
                    };
                self.write_code(quote! {
                    pub fn #conv_to_client_input(input: #server_crate::input::#input_id) -> #client_crate::input::#input_id {
                        #conv_to_client_input_gen
                    }
                });
            }
            {
                let output_id = format_ident!("{}Output", op.name);
                let conv_from_client_output =
                    format_ident!("conv_from_client_{}", snake(&output_id.to_string()));
                let conv_from_client_output_gen =
                    if let Some(mut output_member) = op.members.get("output").cloned() {
                        output_member.name = format!("output/{}", output_id);
                        self.gen_conv_from_client(&output_member, quote! { output  })
                    } else {
                        quote! { #server_crate::output::#output_id {} }
                    };
                self.write_code(quote! {
                    pub fn #conv_from_client_output(output: #client_crate::output::#output_id) -> #server_crate::output::#output_id {
                        #conv_from_client_output_gen
                    }
                });
            }
        }
    }

    pub fn gen_conv_to_client(&mut self, member: &SmithyMember, from: TokenStream) -> TokenStream {
        let client_crate = format_ident!("{}", self.client_crate);
        let server_crate = format_ident!("{}", self.server_crate);
        let shape = self.model.get_shape_of(member);
        let member_split: Vec<_> = member.name.split('/').collect();
        let pkg_name = format_ident!(
            "{}",
            match member_split[0] {
                "input" => "input",
                "output" => "output",
                _ => "model",
            }
        );
        let type_name = format_ident!(
            "{}",
            match member_split[0] {
                "input" => member_split[1],
                "output" => member_split[1],
                _ => shape.name.as_str(),
            }
        );

        match shape.typ {
            SmithyType::Structure => {
                let members: Vec<TokenStream> = shape
                    .members
                    .values()
                    .map(|m| {
                        let m_ident = m.ident();
                        let set_ident = m.set_ident();
                        let s = self.model.get_shape_of(m);
                        let convert = if m.has_trait(SM_REQUIRED) || s.typ.is_always_required() {
                            let c = self.gen_conv_to_client(m, quote! { v. #m_ident });
                            quote! { Some(#c)}
                        } else {
                            let c = self.gen_conv_to_client(m, quote! { v });
                            quote! { v. #m_ident .map(|v| #c) }
                        };
                        quote! { b = b.#set_ident(#convert); }
                    })
                    .collect();
                let build_it = if pkg_name == "input" {
                    quote! { b.build().unwrap() }
                } else {
                    quote! { b.build() }
                };
                quote! {{
                    let v = #from;
                    let mut b = #client_crate::#pkg_name::#type_name::builder();
                    #(#members)*
                    #build_it
                }}
            }

            SmithyType::List => {
                let m = &shape.members["member"];
                let s = self.model.get_shape_of(m);
                let convert = self.gen_conv_to_client(m, quote! { v });
                if s.typ.is_always_required()
                    || (s.typ == SmithyType::String && !s.has_trait(SM_ENUM))
                {
                    quote! { #from .clone() }
                } else {
                    quote! { #from .into_iter().map(|v| #convert).collect() }
                }
            }

            // SmithyType::Map => {}
            SmithyType::Union => {
                let members: Vec<TokenStream> = shape
                    .members
                    .values()
                    .map(|m| {
                        let enum_name = format_ident!("{}", m.name);
                        let c = self.gen_conv_to_client(m, quote! { v });
                        quote! {
                            #server_crate::#pkg_name::#type_name::#enum_name(v) =>
                                #client_crate::#pkg_name::#type_name::#enum_name(#c),
                        }
                    })
                    .collect();
                quote! {{
                    match #from {
                        #(#members)*
                        _ => panic!("unknown union value"),
                    }
                }}
            }

            SmithyType::Blob => {
                quote! { #from }
            }

            SmithyType::String => {
                if shape.has_trait(SM_ENUM) {
                    quote! { #client_crate::#pkg_name::#type_name::from(#from .as_str()) }
                } else {
                    quote! { #from .to_owned() }
                }
            }

            _ => {
                quote! { #from .to_owned() }
            }
        }
    }

    pub fn gen_conv_from_client(
        &mut self,
        member: &SmithyMember,
        from: TokenStream,
    ) -> TokenStream {
        let client_crate = format_ident!("{}", self.client_crate);
        let server_crate = format_ident!("{}", self.server_crate);
        let shape = self.model.get_shape_of(member);
        let member_split: Vec<_> = member.name.split('/').collect();
        let pkg_name = format_ident!(
            "{}",
            match member_split[0] {
                "input" => "input",
                "output" => "output",
                _ => "model",
            }
        );
        let type_name = format_ident!(
            "{}",
            match member_split[0] {
                "input" => member_split[1],
                "output" => member_split[1],
                _ => shape.name.as_str(),
            }
        );

        match shape.typ {
            SmithyType::Structure => {
                let mut has_required = false;
                let members: Vec<TokenStream> = shape
                    .members
                    .values()
                    .map(|m| {
                        let m_ident = m.ident();
                        let set_ident = m.set_ident();
                        let s = self.model.get_shape_of(m);
                        if m.has_trait(SM_REQUIRED)
                            && !s.typ.is_always_required()
                            && (s.typ != SmithyType::String || s.has_trait(SM_ENUM))
                        {
                            has_required = true;
                        }
                        let convert = if s.typ.is_always_required() {
                            let c = self.gen_conv_from_client(m, quote! { v. #m_ident });
                            quote! { Some(#c)}
                        } else {
                            let c = self.gen_conv_from_client(m, quote! { v });
                            quote! { v. #m_ident .map(|v| #c) }
                        };
                        quote! { b = b.#set_ident(#convert); }
                    })
                    .collect();
                let build_it = if pkg_name == "input" || has_required {
                    quote! { b.build().unwrap() }
                } else {
                    quote! { b.build() }
                };
                quote! {{
                    let v = #from;
                    let mut b = #server_crate::#pkg_name::#type_name::builder();
                    #(#members)*
                    #build_it
                }}
            }

            SmithyType::List => {
                let m = &shape.members["member"];
                let s = self.model.get_shape_of(m);
                let convert = self.gen_conv_from_client(m, quote! { v });
                if s.typ.is_always_required()
                    || (s.typ == SmithyType::String && !s.has_trait(SM_ENUM))
                {
                    quote! { #from .clone() }
                } else {
                    quote! { #from .into_iter().map(|v| #convert).collect() }
                }
            }

            // SmithyType::Map => {}
            SmithyType::Union => {
                let members: Vec<TokenStream> = shape
                    .members
                    .values()
                    .map(|m| {
                        let enum_name = format_ident!("{}", m.name);
                        let c = self.gen_conv_from_client(m, quote! { v });
                        quote! {
                            #client_crate::#pkg_name::#type_name::#enum_name(v) =>
                                #server_crate::#pkg_name::#type_name::#enum_name(#c),
                        }
                    })
                    .collect();
                quote! {{
                    match #from {
                        #(#members)*
                        _ => panic!("unknown union value"),
                    }
                }}
            }

            SmithyType::Blob => {
                quote! { #from }
            }

            SmithyType::String => {
                if shape.has_trait(SM_ENUM) {
                    quote! { #server_crate::#pkg_name::#type_name::from(#from .as_str()) }
                } else {
                    quote! { #from .to_owned() }
                }
            }

            _ => {
                quote! { #from .to_owned() }
            }
        }
    }

    pub fn close_output_file(&mut self) {
        let w = self.writer.take();
        if w.is_some() {
            w.unwrap().flush().unwrap();
        }
    }

    pub fn set_output_file(&mut self, fname: &str) {
        self.close_output_file();
        self.writer = Some(CodeWriter::new(&self.out_path.join(fname)));
    }

    fn write_code<T: ToString>(&mut self, code: T) {
        let w = self.writer.as_mut().unwrap();
        w.write_all(code.to_string().as_bytes()).unwrap();
        w.write_all(b"\n\n").unwrap();
    }

    fn _writeln<T: AsRef<[u8]>>(&mut self, s: T) {
        let w = self.writer.as_mut().unwrap();
        w.write_all(s.as_ref()).unwrap();
        w.write_all(b"\n").unwrap();
    }
}

/// CodeWriter pipes generated code through rustfmt
/// and then into an output file.
pub struct CodeWriter {
    path: PathBuf,
    w: Option<BufWriter<File>>,
    // rustfmt: Option<Child>,
}

impl CodeWriter {
    fn new(file_path: &Path) -> Self {
        println!("CodeWriter file {:?}", file_path);
        let file = File::create(file_path).unwrap();
        // let mut rustfmt = Command::new("rustfmt")
        //     .arg("--edition=2021")
        //     .stdin(Stdio::piped())
        //     .stdout(file)
        //     .spawn()
        //     .ok();
        // println!("CodeWriter rustfmt {:?}", rustfmt);
        // BufWriter::new(rustfmt.unwrap().stdin.take().unwrap());
        let w = BufWriter::new(file);
        CodeWriter {
            path: file_path.to_path_buf(),
            w: Some(w),
            // rustfmt,
        }
    }
}

impl Write for CodeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.w.as_mut().unwrap().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        println!("CodeWriter flush buffers {}", self.path.display());
        self.w.take().unwrap().flush()?;
        // println!("CodeWriter wait rustfmt {}", self.path.display());
        // self.rustfmt.take().unwrap().wait()?;
        println!("CodeWriter done {}", self.path.display());
        Ok(())
    }
}

/// SmithyModel is a wrapper around the smithy JSON AST model
/// which provides a convenient interface to read the model
#[derive(Debug, Clone)]
pub struct SmithyModel {
    pub shapes: SmithyShapeMap,
}

impl FromJson for SmithyModel {
    fn from_json(json: &Value) -> Self {
        let shapes = SmithyShapeMap::from_json(&json["shapes"]);
        SmithyModel { shapes }
    }
}
impl SmithyModel {
    pub fn get_shape_of<'a>(&'a self, member: &'a SmithyMember) -> &'a SmithyShape {
        &self.shapes[&member.target]
    }
    pub fn get_shape_if<'a>(&'a self, member: Option<&'a SmithyMember>) -> Option<&'a SmithyShape> {
        member.map(|m| self.get_shape_of(m))
    }
    pub fn get_shape_by_key<'a>(&'a self, k: &str) -> &'a SmithyShape {
        &self.shapes[k]
    }
    pub fn iter_shapes_by_type<'a>(
        &'a self,
        t: SmithyType,
    ) -> impl Iterator<Item = &'a SmithyShape> + 'a {
        self.shapes.values().filter(move |s| s.typ == t)
    }
    pub fn iter_shapes_with_trait<'a>(
        &'a self,
        t: &'a str,
    ) -> impl Iterator<Item = &'a SmithyShape> + 'a {
        self.shapes.values().filter(|s| s.has_trait(t))
    }
}

type JsonObject = Map<String, Value>;

pub trait FromJson {
    fn from_json(json: &Value) -> Self;
}

pub trait SmithyTraitor {
    fn traits(&self) -> &Value;
    fn traits_mut(&mut self) -> &mut Value;
}
pub trait SmithyTraits {
    fn set_trait(&mut self, t: &str);
    fn has_trait(&self, t: &str) -> bool;
    fn get_trait(&self, t: &str) -> String;
    fn get_trait_value(&self, t: &str) -> Value;
    fn has_http_trait(&self) -> bool {
        self.has_trait(SM_HTTP_LABEL)
            || self.has_trait(SM_HTTP_QUERY)
            || self.has_trait(SM_HTTP_HEADER)
            || self.has_trait(SM_HTTP_PREFIX_HEADERS)
    }
}
impl<T: SmithyTraitor> SmithyTraits for T {
    fn set_trait(&mut self, t: &str) {
        self.traits_mut()
            .as_object_mut()
            .map(|o| o.insert(t.to_string(), Value::Object(JsonObject::new())));
    }
    fn has_trait(&self, t: &str) -> bool {
        self.traits()
            .as_object()
            .map_or(false, |o| o.contains_key(t))
    }
    fn get_trait(&self, t: &str) -> String {
        self.traits()
            .as_object()
            .and_then(|o| o.get(t))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default()
    }
    fn get_trait_value(&self, t: &str) -> Value {
        self.traits()
            .as_object()
            .and_then(|o| o.get(t))
            .map_or(Value::Null, |v| v.to_owned())
    }
}

type SmithyShapeMap = HashMap<String, SmithyShape>;
type SmithyMemberMap = HashMap<String, SmithyMember>;

#[derive(Debug, Clone)]
pub struct SmithyShape {
    pub key: String,
    pub name: String,
    pub typ: SmithyType,
    pub traits: Value,
    pub members: SmithyMemberMap,
}
impl SmithyTraitor for SmithyShape {
    fn traits(&self) -> &Value {
        &self.traits
    }
    fn traits_mut(&mut self) -> &mut Value {
        &mut self.traits
    }
}
impl SmithyShape {
    pub fn new(json: &Value, key: &str) -> Self {
        let typ = SmithyType::from(json["type"].as_str().unwrap());
        let traits = json["traits"].to_owned();
        let mut members = SmithyMemberMap::from_json(&json["members"]);
        for k in ["input", "output", "member", "key", "value"].iter() {
            if json[k].is_object() {
                members.insert(k.to_string(), SmithyMember::new(k, &json[k]));
            }
        }
        // TODO json["errors"].as_array()
        // TODO json["operations"].as_array()
        Self {
            key: key.to_string(),
            name: camel(&unprefix(key)),
            typ,
            traits,
            members,
        }
    }
    pub fn ident(&self) -> Ident {
        format_ident!("{}", self.name)
    }
    pub fn get_type(&self) -> &str {
        self.typ.as_ref()
    }
}

#[derive(Debug, Clone)]
pub struct SmithyMember {
    pub name: String,
    pub snake: String,
    pub traits: Value,
    pub target: String,
}
impl SmithyTraitor for SmithyMember {
    fn traits(&self) -> &Value {
        &self.traits
    }
    fn traits_mut(&mut self) -> &mut Value {
        &mut self.traits
    }
}
impl SmithyMember {
    pub fn new(key: &str, json: &Value) -> Self {
        let traits = json["traits"].to_owned();
        let target = json["target"].as_str().unwrap_or("").to_string();
        Self {
            name: key.to_string(),
            snake: snake(key),
            traits,
            target,
        }
    }
    pub fn ident(&self) -> Ident {
        format_ident!("r#{}", self.snake)
    }
    pub fn set_ident(&self) -> Ident {
        format_ident!("set_{}", self.snake)
    }
    pub fn get_ident(&self) -> Ident {
        format_ident!("get_{}", self.snake)
    }
}

/// unprefix returns just the suffix for `prefix#suffix` strings
fn unprefix(s: &str) -> String {
    s.split_once('#')
        .map_or_else(|| s.to_string(), |(_prefix, suffix)| suffix.to_string())
}

/// camel changes from MIXOfUPPERCaseAndCamelCase to MixOfUpperCaseAndCamelCase
fn camel(s: &str) -> String {
    let mut out = String::new();
    let mut upper_streak = 0;
    for c in s.chars() {
        if c.is_uppercase() || c.is_numeric() {
            if upper_streak == 0 {
                out.push(c);
            } else {
                out.push(c.to_lowercase().next().unwrap());
            }
            upper_streak += 1;
        } else {
            if upper_streak > 1 && out.len() > 1 {
                let c = out.pop().unwrap();
                out.push(c.to_uppercase().next().unwrap());
            }
            out.push(c);
            upper_streak = 0;
        }
    }
    out
}

/// snake changes from CamelCase to snake_case
fn snake(s: &str) -> String {
    let mut out = String::new();
    let mut upper_streak = 0;
    for mut c in s.chars() {
        if c.is_uppercase() || c.is_numeric() {
            if upper_streak == 0 && out.len() > 0 && out.chars().last().unwrap() != '_' {
                out.push('_');
            }
            out.push(c.to_lowercase().next().unwrap());
            upper_streak += 1;
        } else {
            if !c.is_alphanumeric() {
                c = '_';
            }
            if upper_streak > 1 && out.len() > 1 && c != '_' {
                let c = out.pop().unwrap();
                out.push('_');
                out.push(c);
            }
            out.push(c);
            upper_streak = 0;
        }
    }
    out
}
impl FromJson for SmithyShapeMap {
    fn from_json(v: &Value) -> Self {
        v.as_object().map_or_else(
            || SmithyShapeMap::new(),
            |m| {
                m.iter()
                    .map(|(k, v)| (k.to_owned(), SmithyShape::new(v, k)))
                    .collect()
            },
        )
    }
}
impl FromJson for SmithyMemberMap {
    fn from_json(v: &Value) -> Self {
        v.as_object().map_or_else(
            || SmithyMemberMap::new(),
            |m| {
                m.iter()
                    .map(|(k, v)| (k.to_owned(), SmithyMember::new(k, v)))
                    .collect()
            },
        )
    }
}

/// smithy shape types
/// https://awslabs.github.io/smithy/1.0/spec/core/model.html#
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmithyType {
    // primitive-shapes
    Boolean,
    Byte,
    Short,
    Integer,
    Long,
    Float,
    Double,
    BigInteger,
    BigDecimal,
    // basic-shapes
    Blob,
    String,
    Timestamp,
    Document,
    // aggregate-shapes
    Member,
    List,
    Set,
    Map,
    Structure,
    Union,
    // service-shapes
    Service,
    Operation,
    Resource,
}
impl SmithyType {
    pub fn is_always_required(&self) -> bool {
        match self {
            SmithyType::Blob
            | SmithyType::Boolean
            | SmithyType::Byte
            | SmithyType::Short
            | SmithyType::Integer
            | SmithyType::Long
            | SmithyType::Float
            | SmithyType::Double
            | SmithyType::BigInteger
            | SmithyType::BigDecimal => true,
            _ => false,
        }
    }
}
impl AsRef<str> for SmithyType {
    fn as_ref(&self) -> &str {
        match self {
            // primitive-shapes
            SmithyType::Boolean => SM_TYPE_BOOLEAN,
            SmithyType::Byte => SM_TYPE_BYTE,
            SmithyType::Short => SM_TYPE_SHORT,
            SmithyType::Integer => SM_TYPE_INTEGER,
            SmithyType::Long => SM_TYPE_LONG,
            SmithyType::Float => SM_TYPE_FLOAT,
            SmithyType::Double => SM_TYPE_DOUBLE,
            SmithyType::BigInteger => SM_TYPE_BIGINTEGER,
            SmithyType::BigDecimal => SM_TYPE_BIGDECIMAL,
            // basic-shapes
            SmithyType::Blob => SM_TYPE_BLOB,
            SmithyType::String => SM_TYPE_STRING,
            SmithyType::Timestamp => SM_TYPE_TIMESTAMP,
            SmithyType::Document => SM_TYPE_DOCUMENT,
            // aggregate-shapes
            SmithyType::Member => SM_TYPE_MEMBER,
            SmithyType::List => SM_TYPE_LIST,
            SmithyType::Set => SM_TYPE_SET,
            SmithyType::Map => SM_TYPE_MAP,
            SmithyType::Structure => SM_TYPE_STRUCTURE,
            SmithyType::Union => SM_TYPE_UNION,
            // service-shapes
            SmithyType::Service => SM_TYPE_SERVICE,
            SmithyType::Operation => SM_TYPE_OPERATION,
            SmithyType::Resource => SM_TYPE_RESOURCE,
        }
    }
}
impl From<&str> for SmithyType {
    fn from(s: &str) -> Self {
        match s {
            // primitive-shapes
            SM_TYPE_BOOLEAN => SmithyType::Boolean,
            SM_TYPE_BYTE => SmithyType::Byte,
            SM_TYPE_SHORT => SmithyType::Short,
            SM_TYPE_INTEGER => SmithyType::Integer,
            SM_TYPE_LONG => SmithyType::Long,
            SM_TYPE_FLOAT => SmithyType::Float,
            SM_TYPE_DOUBLE => SmithyType::Double,
            SM_TYPE_BIGINTEGER => SmithyType::BigInteger,
            SM_TYPE_BIGDECIMAL => SmithyType::BigDecimal,
            // basic-shapes
            SM_TYPE_BLOB => SmithyType::Blob,
            SM_TYPE_STRING => SmithyType::String,
            SM_TYPE_TIMESTAMP => SmithyType::Timestamp,
            SM_TYPE_DOCUMENT => SmithyType::Document,
            // aggregate-shapes
            SM_TYPE_MEMBER => SmithyType::Member,
            SM_TYPE_LIST => SmithyType::List,
            SM_TYPE_SET => SmithyType::Set,
            SM_TYPE_MAP => SmithyType::Map,
            SM_TYPE_STRUCTURE => SmithyType::Structure,
            SM_TYPE_UNION => SmithyType::Union,
            // service-shapes
            SM_TYPE_SERVICE => SmithyType::Service,
            SM_TYPE_OPERATION => SmithyType::Operation,
            SM_TYPE_RESOURCE => SmithyType::Resource,
            _ => panic!("unknown SmithyType: {}", s),
        }
    }
}
impl ToString for SmithyType {
    fn to_string(&self) -> String {
        self.as_ref().to_string()
    }
}

// primitive-shapes
const SM_TYPE_BOOLEAN: &str = "boolean";
const SM_TYPE_BYTE: &str = "byte";
const SM_TYPE_SHORT: &str = "short";
const SM_TYPE_INTEGER: &str = "integer";
const SM_TYPE_LONG: &str = "long";
const SM_TYPE_FLOAT: &str = "float";
const SM_TYPE_DOUBLE: &str = "double";
const SM_TYPE_BIGINTEGER: &str = "bigInteger";
const SM_TYPE_BIGDECIMAL: &str = "bigDecimal";
// basic-shapes
const SM_TYPE_BLOB: &str = "blob";
const SM_TYPE_STRING: &str = "string";
const SM_TYPE_TIMESTAMP: &str = "timestamp";
const SM_TYPE_DOCUMENT: &str = "document";
// aggregate-shapes
const SM_TYPE_MEMBER: &str = "member";
const SM_TYPE_LIST: &str = "list";
const SM_TYPE_SET: &str = "set";
const SM_TYPE_MAP: &str = "map";
const SM_TYPE_STRUCTURE: &str = "structure";
const SM_TYPE_UNION: &str = "union";
// service-shapes
const SM_TYPE_SERVICE: &str = "service";
const SM_TYPE_OPERATION: &str = "operation";
const SM_TYPE_RESOURCE: &str = "resource";

// smithy traits used in s3.json:
const _SM_PREFIX: &str = "smithy.api#";
const _SM_DOC: &str = "smithy.api#documentation";
const SM_ENUM: &str = "smithy.api#enum";
const _SM_ERROR: &str = "smithy.api#error";
const SM_REQUIRED: &str = "smithy.api#required";
const _SM_HTTP: &str = "smithy.api#http";
const SM_HTTP_LABEL: &str = "smithy.api#httpLabel";
const SM_HTTP_QUERY: &str = "smithy.api#httpQuery";
const SM_HTTP_HEADER: &str = "smithy.api#httpHeader";
const _SM_HTTP_PAYLOAD: &str = "smithy.api#httpPayload";
const SM_HTTP_PREFIX_HEADERS: &str = "smithy.api#httpPrefixHeaders";
const _SM_HTTP_CHECKSUM_REQUIRED: &str = "smithy.api#httpChecksumRequired";
const _SM_XML_NS: &str = "smithy.api#xmlNamespace";
const _SM_XML_NAME: &str = "smithy.api#xmlName";
const _SM_XML_ATTR: &str = "smithy.api#xmlAttribute";
const _SM_XML_FLATTENED: &str = "smithy.api#xmlFlattened";
const _SM_SENSITIVE: &str = "smithy.api#sensitive";
const _SM_TIMESTAMP_FORMAT: &str = "smithy.api#timestampFormat";
const _SM_EVENT_PAYLOAD: &str = "smithy.api#eventPayload";
const _SM_STREAMING: &str = "smithy.api#streaming";
const _SM_PAGINATED: &str = "smithy.api#paginated";
const _SM_DEPRECATED: &str = "smithy.api#deprecated";
const _SM_TITLE: &str = "smithy.api#title";
const _SM_PATTERN: &str = "smithy.api#pattern";
const _SM_LENGTH: &str = "smithy.api#length";
const _SM_HOST_LABEL: &str = "smithy.api#hostLabel";
const _SM_ENDPOINT: &str = "smithy.api#endpoint";
const _SM_AUTH: &str = "smithy.api#auth";
