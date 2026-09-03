use anyhow::{Context, Result};
use clap::Parser as ClapParser;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tree_sitter::{Language, Node, Parser};

#[derive(ClapParser)]
#[command(name = "pojo2json")]
#[command(about = "Generate random sample JSON data deserializable to a Java POJO")]
struct Cli {
    /// Java source file to parse
    file: PathBuf,

    /// Print the raw CST (S-expression) instead of JSON
    #[arg(long)]
    tree: bool,
}

// ── Generation model ─────────────────────────────────────────────────

#[derive(Clone)]
struct FieldDef {
    type_name: String,
    name: String,
}

#[derive(Clone)]
struct GenClass {
    fields: Vec<FieldDef>,
}

struct Ctx {
    classes: HashMap<String, GenClass>,
    enums: HashMap<String, Vec<String>>,
    rng: Rng,
}

// Small deterministic PRNG (xorshift64) so we avoid a runtime dependency.
struct Rng {
    state: u64,
}

impl Rng {
    fn seeded() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E3779B97F4A7C15);
        Rng { state: nanos | 1 }
    }

    fn next(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn r#bool(&mut self) -> bool {
        self.next() & 1 == 1
    }

    fn int(&mut self) -> i64 {
        self.below(200_000) as i64 - 100_000
    }

    fn float(&mut self) -> f64 {
        let a = self.below(1_000_000) as f64;
        let b = self.below(1_000_000) as f64;
        (a / b * 10_000.0 * 100.0).floor() / 100.0
    }

    fn letter_lower(&mut self) -> char {
        (b'a' + self.below(26) as u8) as char
    }

    fn string(&mut self, max_len: u64) -> String {
        let len = 3 + self.below(max_len.max(1));
        (0..len).map(|_| self.letter_lower()).collect()
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }

    fn local_date(&mut self) -> String {
        let y = 2020 + self.below(8);
        let m = 1 + self.below(12);
        let d = 1 + self.below(28);
        format!("{y:04}-{m:02}-{d:02}")
    }

    fn local_time(&mut self) -> String {
        let h = self.below(24);
        let mi = self.below(60);
        let s = self.below(60);
        let ms = self.below(1000);
        format!("{h:02}:{mi:02}:{s:02}.{ms:03}")
    }

    fn local_date_time(&mut self) -> String {
        format!("{}T{}", self.local_date(), self.local_time())
    }

    fn instant(&mut self) -> String {
        format!("{}Z", self.local_date_time())
    }

    fn uuid(&mut self) -> String {
        let hex = |n: u64| format!("{n:08x}");
        format!(
            "{}-{}-{}-{}-{}",
            hex(self.below(u64::MAX)),
            hex(self.below(u64::MAX) & 0xFFFF),
            hex(self.below(u64::MAX) & 0xFFFF),
            hex(self.below(u64::MAX) & 0xFFFF),
            hex(self.below(u64::MAX))
        )
    }
}

// ── Type parsing ─────────────────────────────────────────────────────

#[derive(Default)]
struct TypeDesc {
    base: String,
    args: Vec<String>,
    is_array: bool,
}

fn parse_type(typ: &str) -> TypeDesc {
    let mut base = typ.trim();
    let mut is_array = false;
    while let Some(stripped) = base.strip_suffix("[]") {
        base = stripped.trim();
        is_array = true;
    }

    if let Some(lt) = base.find('<') {
        let name = base[..lt].trim().to_string();
        let inner = &base[lt + 1..];
        let mut depth = 1i32;
        for (i, ch) in inner.char_indices() {
            match ch {
                '<' => depth += 1,
                '>' => {
                    depth -= 1;
                    if depth == 0 {
                        let args = split_top_level(&inner[..i]);
                        return TypeDesc {
                            base: name,
                            args,
                            is_array,
                        };
                    }
                }
                _ => {}
            }
        }
        TypeDesc {
            base: name,
            args: Vec::new(),
            is_array,
        }
    } else {
        TypeDesc {
            base: base.to_string(),
            args: Vec::new(),
            is_array,
        }
    }
}

fn split_top_level(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for ch in s.chars() {
        match ch {
            '<' | '[' => {
                depth += 1;
                cur.push(ch);
            }
            '>' | ']' => {
                depth -= 1;
                cur.push(ch);
            }
            ',' if depth == 0 => {
                let t = cur.trim();
                if !t.is_empty() {
                    out.push(t.to_string());
                }
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    let t = cur.trim();
    if !t.is_empty() {
        out.push(t.to_string());
    }
    out
}

// ── Value generation ─────────────────────────────────────────────────

fn float_value(v: f64) -> serde_json::Number {
    serde_json::Number::from_f64(v).unwrap_or_else(|| serde_json::Number::from(0))
}

fn generate_value(ctx: &mut Ctx, typ: &str) -> Value {
    let desc = parse_type(typ);

    if desc.is_array {
        let n = ctx.rng.below(3) as usize;
        let mut arr = Vec::with_capacity(n);
        for _ in 0..n {
            arr.push(generate_value(ctx, &desc.base));
        }
        return Value::Array(arr);
    }

    match desc.base.as_str() {
        "boolean" | "Boolean" => Value::Bool(ctx.rng.r#bool()),
        "byte" | "short" | "int" => Value::Number(ctx.rng.int().into()),
        "Byte" | "Short" | "Integer" => Value::Number(ctx.rng.int().into()),
        "long" => Value::Number(ctx.rng.int().into()),
        "Long" => Value::Number(ctx.rng.int().into()),
        "float" | "double" => Value::Number(float_value(ctx.rng.float())),
        "Float" | "Double" => Value::Number(float_value(ctx.rng.float())),
        "char" | "Character" => Value::String(ctx.rng.letter_lower().to_string()),
        "String" => Value::String(ctx.rng.string(12)),
        "BigInteger" => Value::String(ctx.rng.int().to_string()),
        "BigDecimal" => Value::String(format!("{:.2}", ctx.rng.float())),
        "LocalDate" => Value::String(ctx.rng.local_date()),
        "LocalTime" => Value::String(ctx.rng.local_time()),
        "LocalDateTime" => Value::String(ctx.rng.local_date_time()),
        "Instant" | "OffsetDateTime" | "ZonedDateTime" => Value::String(ctx.rng.instant()),
        "UUID" => Value::String(ctx.rng.uuid()),
        "List" | "Set" | "Collection" | "ArrayList" | "LinkedList" | "HashSet"
        | "LinkedHashSet" | "TreeSet" => {
            let elem = desc
                .args
                .first()
                .cloned()
                .unwrap_or_else(|| "Object".into());
            let n = ctx.rng.below(4) as usize;
            let mut arr = Vec::with_capacity(n);
            for _ in 0..n {
                arr.push(generate_value(ctx, &elem));
            }
            Value::Array(arr)
        }
        "Map" | "HashMap" | "LinkedHashMap" | "TreeMap" => {
            let key = desc
                .args
                .first()
                .cloned()
                .unwrap_or_else(|| "String".into());
            let val = desc.args.get(1).cloned().unwrap_or_else(|| "Object".into());
            let n = ctx.rng.below(3) as usize;
            let mut m = Map::new();
            for _ in 0..n {
                let k = generate_map_key(ctx, &key);
                m.insert(k, generate_value(ctx, &val));
            }
            Value::Object(m)
        }
        "Optional" => {
            if ctx.rng.r#bool() {
                let inner = desc
                    .args
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "Object".into());
                generate_value(ctx, &inner)
            } else {
                Value::Null
            }
        }
        "Object" | "Number" => Value::Null,
        _ => {
            if let Some(class) = ctx.classes.get(&desc.base).cloned() {
                return generate_object(ctx, &class);
            }
            if let Some(constants) = ctx.enums.get(&desc.base) {
                if constants.is_empty() {
                    return Value::String(String::new());
                }
                return Value::String(ctx.rng.pick(constants).clone());
            }
            Value::Null
        }
    }
}

fn generate_map_key(ctx: &mut Ctx, typ: &str) -> String {
    let desc = parse_type(typ);
    match desc.base.as_str() {
        "boolean" | "Boolean" => ctx.rng.r#bool().to_string(),
        "byte" | "short" | "int" | "Integer" | "Byte" | "Short" => ctx.rng.int().to_string(),
        "long" | "Long" => ctx.rng.int().to_string(),
        "float" | "double" | "Float" | "Double" => format!("{:.2}", ctx.rng.float()),
        "char" | "Character" => ctx.rng.letter_lower().to_string(),
        _ => ctx.rng.string(10),
    }
}

fn generate_object(ctx: &mut Ctx, class: &GenClass) -> Value {
    let mut m = Map::new();
    for f in &class.fields {
        let mut key = f.name.clone();
        if m.contains_key(&key) {
            key = format!("{key}_{}", ctx.rng.below(99));
        }
        m.insert(key, generate_value(ctx, &f.type_name));
    }
    Value::Object(m)
}

// ── CST helpers ──────────────────────────────────────────────────────

fn node_text<'a>(node: Node<'a>, source: &'a str) -> String {
    node.utf8_text(source.as_bytes()).unwrap_or("").to_string()
}

fn child_by_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let n = node.named_child_count();
    for i in 0..n {
        if let Some(c) = node.named_child(i as u32) {
            if c.kind() == kind {
                return Some(c);
            }
        }
    }
    None
}

fn fields_of_class<'a>(body: Node<'a>, source: &'a str) -> Vec<FieldDef> {
    let mut fields = Vec::new();
    let n = body.named_child_count();
    for i in 0..n {
        if let Some(c) = body.named_child(i as u32) {
            if c.kind() != "field_declaration" {
                continue;
            }
            let typ = c
                .child_by_field_name("type")
                .map(|t| node_text(t, source).trim().to_string())
                .unwrap_or_default();
            let dn = c.named_child_count();
            for j in 0..dn {
                if let Some(dec) = c.named_child(j as u32) {
                    if dec.kind() == "variable_declarator" {
                        let name = dec
                            .child_by_field_name("name")
                            .map(|x| node_text(x, source))
                            .unwrap_or_default();
                        fields.push(FieldDef {
                            type_name: typ.clone(),
                            name,
                        });
                    }
                }
            }
        }
    }
    fields
}

fn collect_declarations(node: Node, source: &str, ctx: &mut Ctx) {
    match node.kind() {
        "class_declaration" | "interface_declaration" => {
            let name = node
                .child_by_field_name("name")
                .map(|n| node_text(n, source))
                .unwrap_or_default();
            let fields = child_by_kind(node, "class_body")
                .map(|b| fields_of_class(b, source))
                .unwrap_or_default();
            ctx.classes.insert(name.clone(), GenClass { fields });
        }
        "record_declaration" => {
            let name = node
                .child_by_field_name("name")
                .map(|n| node_text(n, source))
                .unwrap_or_default();
            let mut fields = Vec::new();
            if let Some(params) = node.child_by_field_name("parameters") {
                let n = params.named_child_count();
                for i in 0..n {
                    if let Some(p) = params.named_child(i as u32) {
                        if p.kind() == "formal_parameter" || p.kind() == "spread_parameter" {
                            let typ = p
                                .child_by_field_name("type")
                                .map(|t| node_text(t, source).trim().to_string())
                                .unwrap_or_default();
                            let pname = p
                                .child_by_field_name("name")
                                .map(|x| node_text(x, source))
                                .unwrap_or_default();
                            fields.push(FieldDef {
                                type_name: typ,
                                name: pname,
                            });
                        }
                    }
                }
            }
            ctx.classes.insert(name.clone(), GenClass { fields });
        }
        "enum_declaration" => {
            let name = node
                .child_by_field_name("name")
                .map(|n| node_text(n, source))
                .unwrap_or_default();
            let mut constants = Vec::new();
            if let Some(body) = child_by_kind(node, "enum_body") {
                let n = body.named_child_count();
                for i in 0..n {
                    if let Some(c) = body.named_child(i as u32) {
                        if c.kind() == "enum_constant" {
                            if let Some(k) = c.child_by_field_name("name") {
                                constants.push(node_text(k, source));
                            }
                        }
                    }
                }
            }
            ctx.enums.insert(name, constants);
        }
        _ => {}
    }

    let n = node.named_child_count();
    for i in 0..n {
        if let Some(c) = node.named_child(i as u32) {
            collect_declarations(c, source, ctx);
        }
    }
}

fn top_level_classes(root: Node, source: &str) -> Vec<String> {
    let mut names = Vec::new();
    let n = root.named_child_count();
    for i in 0..n {
        if let Some(c) = root.named_child(i as u32) {
            if matches!(
                c.kind(),
                "class_declaration" | "interface_declaration" | "record_declaration"
            ) {
                if let Some(name) = c.child_by_field_name("name") {
                    names.push(node_text(name, source));
                }
            }
        }
    }
    names
}

// ── Main ─────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let cli = Cli::parse();

    let source = fs::read_to_string(&cli.file)
        .with_context(|| format!("failed to read {}", cli.file.display()))?;

    let mut parser = Parser::new();
    let lang: Language = tree_sitter_java::LANGUAGE.into();
    parser
        .set_language(&lang)
        .context("failed to set Java language")?;

    let tree = parser
        .parse(&source, None)
        .context("failed to parse Java source")?;

    if cli.tree {
        println!("{}", tree.root_node().to_sexp());
        return Ok(());
    }

    let root = tree.root_node();
    let mut ctx = Ctx {
        classes: HashMap::new(),
        enums: HashMap::new(),
        rng: Rng::seeded(),
    };
    collect_declarations(root, &source, &mut ctx);

    let tops = top_level_classes(root, &source);

    let mut objects = Vec::new();
    for name in &tops {
        if let Some(class) = ctx.classes.get(name).cloned() {
            objects.push((name.clone(), generate_object(&mut ctx, &class)));
        }
    }

    let out = if objects.len() == 1 {
        objects.into_iter().next().unwrap().1
    } else {
        let mut m = Map::new();
        for (name, value) in objects {
            m.insert(name, value);
        }
        Value::Object(m)
    };

    println!("{}", serde_json::to_string_pretty(&out)?);

    Ok(())
}
