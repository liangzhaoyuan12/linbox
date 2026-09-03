//! 正则 → 候选 Key 字典生成器（纯逻辑，无 GTK，可 `cargo test`）。
//!
//! ## 为什么自己实现
//! `regex` crate 做的是「匹配」，而这里需要的是「枚举」：把一条正则能表示的
//! 所有字符串按顺序展开成字典。另外本项目保持零新增依赖，因此这里实现了一个
//! 正则子集解析器 + 枚举器。
//!
//! ## 支持的语法
//! ```text
//! alternation := concat ('|' concat)*
//! concat      := repeat*
//! repeat      := atom quantifier?
//! atom        := '(' alternation ')' | '(?:' alternation ')' | '[' class ']' | escape | '.' | literal
//! quantifier  := '*' | '+' | '?' | '{n}' | '{n,}' | '{n,m}'
//! escape      := '\' (d | D | w | W | s | S | n | t | r | f | v | 任意字符)
//! class       := '^'? (range | escape | char)+
//! ```
//! - `^` / `$` 锚点被直接忽略（只影响匹配，不影响生成）。
//! - `.` 与否定字符类 `[^...]` 的取值域是**可打印 ASCII**（`0x20..=0x7E`）。
//!   API Key 不含控制字符，故生成结果中会剔除含控制字符的候选。
//! - `*`、`+`、`{n,}` 是无界量词，按 [`GenerateOptions::unbounded_repeat`] 展开。
//!
//! ## 规模保护
//! 枚举结果条数硬上限为 [`GenerateOptions::max_results`]；超过即按枚举顺序截断。

/// 生成参数。
#[derive(Debug, Clone, Copy)]
pub struct GenerateOptions {
    /// 结果条数上限（>= 1，u128 无硬性上限；实际生成量受运存与
    /// 平台 `usize` 位宽约束，超出部分按平台上限饱和）。
    pub max_results: u128,
    /// 无界量词（`*`、`+`、`{n,}`）的展开上限（>= 1）。
    pub unbounded_repeat: usize,
    /// 是否从整个密钥空间随机采样（true = 每一位独立均匀取值，天然乱序、
    /// 高位不再全是小数字；false = 按枚举顺序取前 `max_results` 条）。
    pub random_sample: bool,
    /// 采样/洗牌种子。同一配置 + 种子每次生成内容与顺序完全一致，
    /// 保证「断点续跑」的游标在下次启动重生成字典后仍然有效。
    pub seed: u64,
}

impl Default for GenerateOptions {
    fn default() -> Self {
        GenerateOptions {
            max_results: 100_000,
            unbounded_repeat: 3,
            random_sample: false,
            seed: 0,
        }
    }
}

/// 由若干配置片段计算采样种子（FNV-1a 64 位），保证相同的
/// 平台正则 + 生成参数在多次运行间产生完全相同的采样结果。
pub fn sample_seed_for(parts: &[&str]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for b in part.as_bytes() {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
        // 段分隔，避免 "a"+"bc" 与 "ab"+"c" 碰撞
        hash ^= 0x1f;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

/// 量词 `{n,m}` 中 m 的绝对上限，防止用户手滑写出 `{999999999}`。
const MAX_REPEAT: usize = 4096;

// ---------------------------------------------------------------------------
// AST
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Node {
    /// 字面串。
    Literal(String),
    /// 单字符取值集合。
    Set(Vec<char>),
    /// 顺序拼接。
    Concat(Vec<Node>),
    /// 分支选择 `(a|b)`。
    Alternate(Vec<Node>),
    /// 重复 `{min,max}`。
    Repeat(Box<Node>, usize, usize),
}

// ---------------------------------------------------------------------------
// 字符集合
// ---------------------------------------------------------------------------

/// `.` 与否定字符类的取值域：可打印 ASCII。
fn universe() -> Vec<char> {
    (0x20u8..=0x7e).map(|b| b as char).collect()
}

fn digits() -> Vec<char> {
    ('0'..='9').collect()
}

fn word_chars() -> Vec<char> {
    let mut v: Vec<char> = ('a'..='z').chain('A'..='Z').chain('0'..='9').collect();
    v.push('_');
    v
}

fn whitespace() -> Vec<char> {
    vec![' ', '\t', '\n', '\r', '\x0b', '\x0c']
}

/// 从全集里挖掉 `excluded`。
fn complement(excluded: &[char]) -> Vec<char> {
    universe().into_iter().filter(|c| !excluded.contains(c)).collect()
}

/// 正则元字符：字面串遇到它们即停止。
///
/// 注意 `]` 与 `}` 不在其中 —— 它们在字符类 / 量词之外就是普通字符，
/// 这样 `a]b`、`a}b` 才能按字面量解析。
fn is_meta(c: char) -> bool {
    matches!(c, '(' | ')' | '[' | '{' | '*' | '+' | '?' | '|' | '.' | '^' | '$' | '\\')
}

// ---------------------------------------------------------------------------
// 解析器
// ---------------------------------------------------------------------------

struct Parser {
    chars: Vec<char>,
    pos: usize,
    unbounded: usize,
}

impl Parser {
    fn new(pattern: &str, unbounded: usize) -> Self {
        Parser {
            chars: pattern.chars().collect(),
            pos: 0,
            unbounded: unbounded.max(1),
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    /// 解析整条模式。
    fn parse(&mut self) -> Result<Node, String> {
        let node = self.parse_alternate()?;
        if let Some(c) = self.peek() {
            return Err(format!("模式第 {} 个字符 `{c}` 无法解析（多余的右括号？）", self.pos + 1));
        }
        Ok(node)
    }

    fn parse_alternate(&mut self) -> Result<Node, String> {
        let mut branches = vec![self.parse_concat()?];
        while self.peek() == Some('|') {
            self.pos += 1;
            branches.push(self.parse_concat()?);
        }
        if branches.len() == 1 {
            Ok(branches.remove(0))
        } else {
            Ok(Node::Alternate(branches))
        }
    }

    fn parse_concat(&mut self) -> Result<Node, String> {
        let mut parts: Vec<Node> = Vec::new();
        loop {
            match self.peek() {
                None | Some('|') | Some(')') => break,
                _ => parts.push(self.parse_repeat()?),
            }
        }
        if parts.is_empty() {
            Ok(Node::Literal(String::new()))
        } else if parts.len() == 1 {
            Ok(parts.remove(0))
        } else {
            Ok(Node::Concat(parts))
        }
    }

    fn parse_repeat(&mut self) -> Result<Node, String> {
        let start = self.pos;
        let atom = self.parse_atom()?;
        match self.parse_quantifier()? {
            Some((min, max)) => {
                if max < min {
                    return Err(format!("量词范围非法（{{ {min},{max} }}），最小值不能大于最大值"));
                }
                if max > MAX_REPEAT {
                    return Err(format!(
                        "量词上限 {max} 过大，最大允许 {MAX_REPEAT}（密钥空间也不具备可枚举性）"
                    ));
                }
                Ok(Node::Repeat(Box::new(atom), min, max))
            }
            None => {
                // 位置没动说明解析异常，避免因死循环卡死
                if self.pos == start {
                    return Err(format!("模式第 {} 个字符无法解析", start + 1));
                }
                Ok(atom)
            }
        }
    }

    /// 读取量词；不是量词则**不消费**任何字符并返回 `None`。
    fn parse_quantifier(&mut self) -> Result<Option<(usize, usize)>, String> {
        match self.peek() {
            Some('*') => {
                self.pos += 1;
                Ok(Some((0, self.unbounded)))
            }
            Some('+') => {
                self.pos += 1;
                Ok(Some((1, self.unbounded)))
            }
            Some('?') => {
                self.pos += 1;
                Ok(Some((0, 1)))
            }
            Some('{') => self.parse_braces(),
            _ => Ok(None),
        }
    }

    /// 解析 `{n}` / `{n,}` / `{n,m}`；格式不合法时把 `{` 当普通字符，返回 `None`。
    fn parse_braces(&mut self) -> Result<Option<(usize, usize)>, String> {
        let save = self.pos;
        self.pos += 1; // 跳过 '{'
        let mut num = String::new();
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                num.push(c);
                self.pos += 1;
            } else {
                break;
            }
        }
        if num.is_empty() {
            self.pos = save;
            return Ok(None);
        }
        let min: usize = num.parse().map_err(|_| "量词数字过大".to_string())?;
        match self.peek() {
            Some('}') => {
                self.pos += 1;
                Ok(Some((min, min)))
            }
            Some(',') => {
                self.pos += 1;
                let mut num2 = String::new();
                while let Some(c) = self.peek() {
                    if c.is_ascii_digit() {
                        num2.push(c);
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                if self.peek() != Some('}') {
                    self.pos = save;
                    return Ok(None);
                }
                self.pos += 1;
                if num2.is_empty() {
                    // {n,} → 无界，按 unbounded 展开
                    Ok(Some((min, min + self.unbounded)))
                } else {
                    let max: usize = num2.parse().map_err(|_| "量词数字过大".to_string())?;
                    Ok(Some((min, max)))
                }
            }
            _ => {
                self.pos = save;
                Ok(None)
            }
        }
    }

    fn parse_atom(&mut self) -> Result<Node, String> {
        match self.peek() {
            None => Err("模式意外结束".into()),
            Some('(') => {
                self.pos += 1;
                // 支持非捕获分组 (?:...)
                if self.chars[self.pos..].starts_with(&['?', ':']) {
                    self.pos += 2;
                }
                let inner = self.parse_alternate()?;
                if self.peek() != Some(')') {
                    return Err("分组缺少右括号 `)`".into());
                }
                self.pos += 1;
                Ok(inner)
            }
            Some('[') => self.parse_class(),
            Some('\\') => self.parse_escape(),
            Some('.') => {
                self.pos += 1;
                Ok(Node::Set(universe()))
            }
            // 锚点：只影响匹配位置，生成时忽略
            Some('^') | Some('$') => {
                self.pos += 1;
                Ok(Node::Literal(String::new()))
            }
            Some('*') | Some('+') | Some('?') => Err(format!(
                "模式第 {} 个字符 `{:?}` 前面缺少可重复的元素",
                self.pos + 1,
                self.peek()
            )),
            Some(_) => self.parse_literal_run(),
        }
    }

    /// 连续读取普通字面字符，合并成一个 `Literal`。
    ///
    /// 关键细节：量词只作用于**紧邻其左侧的那一个原子**。因此当某个字符后面
    /// 紧跟量词时，字面串必须在此处断开，把该字符留给下一轮 `parse_repeat`
    /// 单独成原子，才能让 `sk-a{2}` 解析出 `sk-aa` 而不是 `sk-ask-a`。
    fn parse_literal_run(&mut self) -> Result<Node, String> {
        let mut s = String::new();
        while let Some(c) = self.peek() {
            if is_meta(c) || self.quantifier_at(self.pos + 1) {
                break;
            }
            s.push(c);
            self.pos += 1;
        }
        if s.is_empty() {
            // 首字符就带着量词（或刚好是元字符）：只消费这一个字符
            let c = self.peek().ok_or_else(|| "模式意外结束".to_string())?;
            self.pos += 1;
            s.push(c);
        }
        Ok(Node::Literal(s))
    }

    /// `at` 处是否是一个量词的开头。
    fn quantifier_at(&self, at: usize) -> bool {
        match self.chars.get(at) {
            Some('*') | Some('+') | Some('?') => true,
            Some('{') => self.is_brace_quantifier_at(at),
            _ => false,
        }
    }

    /// `{` 是否构成一个合法量词（`{n}` / `{n,}` / `{n,m}`）。
    ///
    /// 用于区分「量词」与「普通花括号字面量」（如 `a{b}`）。
    fn is_brace_quantifier_at(&self, at: usize) -> bool {
        let mut i = at + 1;
        let mut digits = 0usize;
        while let Some(c) = self.chars.get(i) {
            if c.is_ascii_digit() {
                digits += 1;
                i += 1;
            } else {
                break;
            }
        }
        if digits == 0 {
            return false;
        }
        match self.chars.get(i) {
            Some('}') => true,
            Some(',') => {
                i += 1;
                while let Some(c) = self.chars.get(i) {
                    if c.is_ascii_digit() {
                        i += 1;
                    } else {
                        break;
                    }
                }
                matches!(self.chars.get(i), Some('}'))
            }
            _ => false,
        }
    }

    /// 解析字符类 `[...]`。
    fn parse_class(&mut self) -> Result<Node, String> {
        self.pos += 1; // 跳过 '['
        let negated = self.peek() == Some('^');
        if negated {
            self.pos += 1;
        }
        let mut items: Vec<char> = Vec::new();
        let mut first = true;
        loop {
            match self.peek() {
                None => return Err("字符类缺少右括号 `]`".into()),
                Some(']') if !first => {
                    self.pos += 1;
                    break;
                }
                Some(']') => {
                    // `[]]` 里的首个 `]` 是普通字符
                    items.push(']');
                    self.pos += 1;
                }
                Some('\\') => {
                    let (chars, _) = self.read_escape()?;
                    items.extend(chars);
                }
                Some(c) => {
                    self.pos += 1;
                    // 可能构成区间 a-z
                    if self.peek() == Some('-') && self.chars.get(self.pos + 1) != Some(&']') {
                        self.pos += 1; // 吃掉 '-'
                        let end = match self.peek() {
                            None => return Err("字符类区间缺少右边界".into()),
                            Some('\\') => {
                                let (chars, _) = self.read_escape()?;
                                *chars.first().ok_or("转义区间不合法")?
                            }
                            Some(e) => {
                                self.pos += 1;
                                e
                            }
                        };
                        if end < c {
                            return Err(format!("字符类区间 `{c}-{end}` 顺序颠倒"));
                        }
                        for ch in c..=end {
                            items.push(ch);
                        }
                    } else {
                        items.push(c);
                    }
                }
            }
            first = false;
        }
        items.sort_unstable();
        items.dedup();
        if items.is_empty() {
            return Err("字符类不能为空".into());
        }
        if negated {
            Ok(Node::Set(complement(&items)))
        } else {
            Ok(Node::Set(items))
        }
    }

    /// 解析模式中的 `\x` 转义。
    fn parse_escape(&mut self) -> Result<Node, String> {
        let (chars, is_class) = self.read_escape()?;
        if is_class || chars.len() > 1 {
            Ok(Node::Set(chars))
        } else {
            Ok(Node::Literal(chars[0].to_string()))
        }
    }

    /// 读取一个转义序列，返回 (字符集合, 是否为字符类语义)。
    fn read_escape(&mut self) -> Result<(Vec<char>, bool), String> {
        if self.peek() != Some('\\') {
            return Err("不是转义序列".into());
        }
        self.pos += 1;
        let c = self.peek().ok_or_else(|| "模式以单个反斜杠结尾".to_string())?;
        self.pos += 1;
        Ok(match c {
            'd' => (digits(), true),
            'D' => (complement(&digits()), true),
            'w' => (word_chars(), true),
            'W' => (complement(&word_chars()), true),
            's' => (whitespace(), true),
            'S' => (complement(&whitespace()), true),
            'n' => (vec!['\n'], false),
            't' => (vec!['\t'], false),
            'r' => (vec!['\r'], false),
            'f' => (vec!['\x0c'], false),
            'v' => (vec!['\x0b'], false),
            '0' => (vec!['\0'], false),
            other => (vec![other], false),
        })
    }
}

/// 解析模式为 AST。
fn parse(pattern: &str, unbounded: usize) -> Result<Node, String> {
    Parser::new(pattern, unbounded).parse()
}

// ---------------------------------------------------------------------------
// 枚举
// ---------------------------------------------------------------------------

/// 把 [0, limit) 范围内能枚举出的字符串追加进 `out`。
fn enumerate_into(node: &Node, limit: usize, out: &mut Vec<String>) {
    match node {
        Node::Literal(s) => out.push(s.clone()),
        Node::Set(chars) => {
            for c in chars {
                if out.len() >= limit {
                    return;
                }
                out.push(c.to_string());
            }
        }
        Node::Alternate(branches) => {
            for b in branches {
                if out.len() >= limit {
                    return;
                }
                enumerate_into(b, limit, out);
            }
        }
        Node::Concat(parts) => cartesian(parts, limit, out),
        Node::Repeat(inner, min, max) => {
            for k in *min..=*max {
                if out.len() >= limit {
                    return;
                }
                // k 次重复 = k 重笛卡尔积（k = 0 时给出空串）
                let mut acc: Vec<String> = vec![String::new()];
                for _ in 0..k {
                    let mut items = Vec::new();
                    enumerate_into(inner, limit, &mut items);
                    if items.is_empty() {
                        acc.clear();
                        break;
                    }
                    acc = product(&acc, &items, limit);
                    if acc.is_empty() {
                        break;
                    }
                }
                for s in acc {
                    if out.len() >= limit {
                        return;
                    }
                    out.push(s);
                }
            }
        }
    }
}

/// 两组字符串的笛卡尔积（左侧变化最慢），结果最多 `limit` 条。
fn product(a: &[String], b: &[String], limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    'outer: for x in a {
        for y in b {
            out.push(format!("{x}{y}"));
            if out.len() >= limit {
                break 'outer;
            }
        }
    }
    out
}

/// 按顺序拼接若干节点的枚举结果。
fn cartesian(parts: &[Node], limit: usize, out: &mut Vec<String>) {
    let mut acc: Vec<String> = vec![String::new()];
    for p in parts {
        let mut items = Vec::new();
        enumerate_into(p, limit, &mut items);
        if items.is_empty() {
            return; // 某个片段无法产生任何字符串 → 整体无解
        }
        let mut next = Vec::new();
        'outer: for x in &acc {
            for y in &items {
                next.push(format!("{x}{y}"));
                if next.len() >= limit {
                    break 'outer;
                }
            }
        }
        acc = next;
        if acc.is_empty() {
            return;
        }
    }
    for s in acc {
        if out.len() >= limit {
            return;
        }
        out.push(s);
    }
}

/// 估算模式能枚举出的字符串总数（超出 u128 时饱和）。
fn count(node: &Node) -> u128 {
    match node {
        Node::Literal(_) => 1,
        Node::Set(chars) => chars.len() as u128,
        Node::Alternate(branches) => branches.iter().map(count).fold(0u128, |a, b| a.saturating_add(b)),
        Node::Concat(parts) => parts.iter().map(count).fold(1u128, |a, b| a.saturating_mul(b)),
        Node::Repeat(inner, min, max) => {
            let n = count(inner);
            let mut total = 0u128;
            let mut pow = 1u128;
            for k in 0..=*max {
                if k >= *min {
                    total = total.saturating_add(pow);
                }
                pow = pow.saturating_mul(n);
            }
            total
        }
    }
}

// ---------------------------------------------------------------------------
// 对外 API
// ---------------------------------------------------------------------------

/// 生成失败 / 生成结果。
#[derive(Debug)]
pub struct Dictionary {
    /// 候选 Key 列表（已去重、已剔除含控制字符的项、已按 `max_results` 截断）。
    pub keys: Vec<String>,
    /// 模式本身的完整密钥空间大小（可能远大于 `keys.len()`）。
    pub total_space: u128,
    /// 是否因为达到上限而截断。
    pub truncated: bool,
    /// 因含控制字符而被剔除的候选数。
    pub dropped: usize,
}

/// 当前可用物理内存（Linux `/proc/meminfo` 的 MemAvailable）。
/// 读不到（非 Linux / 特殊环境）时返回 None，此时不做内存闸门、直接放行。
pub fn available_memory_bytes() -> Option<u128> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            let kb: u64 = rest.trim().trim_end_matches("kB").trim().parse().ok()?;
            return Some((kb as u128).saturating_mul(1024));
        }
    }
    None
}

/// 按当前可用运存推荐的最大生成条数：可用内存 ÷ 每条 Key 的字节上界。
/// `None` = 读不到可用内存信息，或当前正则无法解析（此时无法预估）。
pub fn recommended_max_keys(pattern: &str, unbounded: usize) -> Option<u128> {
    let avail = available_memory_bytes()?;
    let node = parse(pattern, unbounded).ok()?;
    let per_key = estimate_key_bytes_upper(&node);
    Some(avail / per_key)
}

/// 估算单条候选 Key 的最大字节数（AST 上界，含 String 结构体与分配器开销）。
fn estimate_key_bytes_upper(node: &Node) -> u128 {
    fn bound(n: &Node) -> u128 {
        match n {
            Node::Literal(s) => s.len() as u128,
            Node::Set(_) => 1,
            Node::Alternate(branches) => branches.iter().map(bound).max().unwrap_or(1),
            Node::Concat(parts) => parts.iter().map(bound).fold(0u128, |a, b| a.saturating_add(b)),
            Node::Repeat(inner, _, max) => bound(inner).saturating_mul(*max as u128),
        }
    }
    // 字符串内容上界 + String 结构体（24B）+ 分配器对齐/头开销（约 40B）
    bound(node).saturating_add(64)
}

fn fmt_gb(bytes: u128) -> String {
    format!("{:.1}", bytes as f64 / 1_000_000_000.0)
}

/// 内存闸门（取代固定「500 万上限」）：没有固定条数限制，宿主机能装多少
/// 就生成多少；仅在估算需求明显超出**当前可用运存**时拒绝。
/// 这是防闪退的最后一道防线——Rust 在分配失败时直接 abort，无法捕获。
fn check_memory(node: &Node, keys: u128) -> Result<(), String> {
    let Some(avail) = available_memory_bytes() else {
        return Ok(()); // 读不到可用内存信息 → 放行（由系统自行承担）
    };
    let per_key = estimate_key_bytes_upper(node);
    let need = per_key.saturating_mul(keys);
    if need > avail {
        return Err(format!(
            "预估需要 {} GB 运存（{} 条 × 每条约 {} 字节），当前可用约 {} GB；请调小「最大生成条数」",
            fmt_gb(need),
            format_count(keys),
            per_key,
            fmt_gb(avail)
        ));
    }
    Ok(())
}

/// 解析 + 生成：把正则展开成候选 Key 字典。
///
/// - `random_sample = false`：按枚举顺序取前 `max_results` 条（兼容旧语义）；
/// - `random_sample = true`：对**任意**正则都从整个密钥空间逐位独立均匀采样
///   （每一位、每一位置的字符独立均匀 → 天然乱序、天然去重，不存在
///   「高位全是 0」这类偏斜）；采样多线程并行（线程数只由 `max_results` 决定，
///   与机器无关，跨会话可复现）。空间 ≤ 上限时改为完整枚举 + 种子洗牌，一个不漏。
///
/// 条数没有固定上限：生成前按当前可用运存估算，装得下就生成，装不下则报错。
/// 采样/洗牌共用 `seed`，同配置每次生成内容与顺序一致，断点续跑游标仍然有效。
pub fn generate(pattern: &str, opts: &GenerateOptions) -> Result<Dictionary, String> {
    let limit_u128 = opts.max_results.max(1);
    // 内存侧实际可用条数：先受平台 usize 位宽约束（u128 输入向下饱和）
    let limit = usize::try_from(limit_u128).unwrap_or(usize::MAX);
    let node = parse(pattern, opts.unbounded_repeat)?;
    let total_space = count(&node);
    let total_usize = usize::try_from(total_space).unwrap_or(usize::MAX);

    let mut keys = Vec::new();
    let mut dropped = 0usize;

    if opts.random_sample && total_space > limit_u128 {
        // 空间比上限大：多线程并行采样，保证分布足够分散
        check_memory(&node, limit_u128)?;
        keys = sample_dict(&node, limit, opts.seed, total_usize);
    } else if opts.random_sample {
        // 空间 ≤ 上限：完整枚举 + 种子洗牌，一个不漏
        check_memory(&node, total_space)?;
        let mut raw = Vec::new();
        enumerate_into(&node, total_usize, &mut raw);
        for k in raw {
            if k.chars().any(|c| c.is_control()) {
                dropped += 1;
                continue;
            }
            keys.push(k);
        }
        if keys.len() > 1 {
            use rand::rngs::StdRng;
            use rand::seq::SliceRandom;
            use rand::SeedableRng;
            keys.shuffle(&mut StdRng::seed_from_u64(opts.seed));
        }
    } else {
        // 顺序模式：按旧语义枚举 limit*2 条再截断去重
        let mut raw = Vec::new();
        enumerate_into(&node, limit.saturating_mul(2), &mut raw);
        let mut seen = std::collections::HashSet::new();
        for k in raw {
            if keys.len() >= limit {
                break;
            }
            if k.chars().any(|c| c.is_control()) {
                dropped += 1;
                continue;
            }
            if seen.insert(k.clone()) {
                keys.push(k);
            }
        }
    }
    let truncated = keys.len() as u128 >= limit_u128 && total_space > keys.len() as u128;

    Ok(Dictionary {
        keys,
        total_space,
        truncated,
        dropped,
    })
}

/// 按 AST 结构逐位随机选取一个字符串：字符类里每个字符独立均匀，
/// 所以长密钥的每一位（含最高位）都均匀分布，而不是枚举顺序导致的
/// 「高位全是 0」。返回 false 表示该分支无法产出字符串（正常 AST 不会发生）。
fn sample_one(node: &Node, rng: &mut rand::rngs::StdRng, out: &mut String) -> bool {
    use rand::Rng;
    match node {
        Node::Literal(s) => {
            out.push_str(s);
            true
        }
        Node::Set(chars) => {
            let i = rng.gen_range(0..chars.len());
            out.push(chars[i]);
            true
        }
        Node::Alternate(branches) => {
            let b = rng.gen_range(0..branches.len());
            sample_one(&branches[b], rng, out)
        }
        Node::Concat(parts) => {
            for p in parts {
                if !sample_one(p, rng, out) {
                    return false;
                }
            }
            true
        }
        Node::Repeat(inner, min, max) => {
            let k = rng.gen_range(*min..=*max);
            for _ in 0..k {
                if !sample_one(inner, rng, out) {
                    return false;
                }
            }
            true
        }
    }
}

/// 单个随机流采样：固定尝试预算，重复只消耗预算不占容量。
fn sample_chunk(node: &Node, want: usize, seed: u64, budget: usize, out: &mut Vec<String>) {
    use rand::rngs::StdRng;
    use rand::SeedableRng;
    let mut rng = StdRng::seed_from_u64(seed);
    let mut attempts = 0usize;
    while out.len() < want && attempts < budget {
        attempts += 1;
        let mut s = String::with_capacity(128);
        if sample_one(node, &mut rng, &mut s) && !s.chars().any(|c| c.is_control()) {
            out.push(s);
        }
    }
}

/// 多线程并行采样：线程数只由 `want` 决定（每 ~4096 条一个，最多 16），
/// 与机器核数无关 → 同一种子在任何机器上都产生一致结果（断点续跑依赖）。
/// 各线程用独立的确定性随机流；合并后用「排序去重」而非全局 HashSet 去重，
/// 内存占用 ≈ 最终字典本身（无重复副本）；不足部分用新种子流补采；
/// 最后再用种子洗牌恢复乱序（排序把顺序破坏了，必须洗回来）。
fn sample_dict(node: &Node, want: usize, seed: u64, total_usize: usize) -> Vec<String> {
    use rand::rngs::StdRng;
    use rand::SeedableRng;
    let threads = want.div_ceil(4096).clamp(1, 16);
    let budget = want.saturating_mul(8).max(4096);

    let mut parts: Vec<Vec<String>> = Vec::with_capacity(threads);
    if threads == 1 {
        let mut v = Vec::with_capacity(want.min(total_usize));
        sample_chunk(node, want, seed, budget, &mut v);
        parts.push(v);
    } else {
        let chunk_want = want.div_ceil(threads);
        let chunk_budget = budget.div_ceil(threads);
        std::thread::scope(|s| {
            let mut handles = Vec::with_capacity(threads);
            for i in 0..threads {
                let seed_i = seed
                    .wrapping_add(i as u64)
                    .wrapping_mul(0x9E37_79B9_7F4A_7C15);
                handles.push(s.spawn(move || {
                    let mut v = Vec::with_capacity(chunk_want);
                    sample_chunk(node, chunk_want, seed_i, chunk_budget, &mut v);
                    v
                }));
            }
            for h in handles {
                parts.push(h.join().expect("字典生成线程崩溃"));
            }
        });
    }

    // 合并 + 排序去重（O(n log n)，比全局 HashSet 省内存）
    let mut keys: Vec<String> = parts.into_iter().flatten().collect();
    keys.sort_unstable();
    keys.dedup();

    // 空间略大于上限时可能差几个：用新种子流补采，直到凑够或空间耗尽
    if keys.len() < want && keys.len() < total_usize {
        let mut seen: std::collections::HashSet<String> = keys.iter().cloned().collect();
        let mut rng = StdRng::seed_from_u64(seed.wrapping_add(0xD1B5_4A32_D192_ED03));
        let mut attempts = 0usize;
        while keys.len() < want && keys.len() < total_usize && attempts < budget {
            attempts += 1;
            let mut s = String::with_capacity(128);
            if sample_one(node, &mut rng, &mut s)
                && !s.chars().any(|c| c.is_control())
                && seen.insert(s.clone())
            {
                keys.push(s);
            }
        }
    }

    // 排序破坏了随机顺序，用种子洗牌恢复（跨会话可复现）
    if keys.len() > 1 {
        use rand::seq::SliceRandom;
        keys.shuffle(&mut StdRng::seed_from_u64(seed));
    }
    keys
}

/// 只估算密钥空间大小，不做枚举（用于给「该模式有多大」的快速反馈）。
pub fn estimate_space(pattern: &str, unbounded_repeat: usize) -> Result<u128, String> {
    Ok(count(&parse(pattern, unbounded_repeat)?))
}

/// 把可能极大的计数格式化成人类可读文本。
pub fn format_count(n: u128) -> String {
    if n == u128::MAX {
        return "超出可估算范围（> 3.4e38）".to_string();
    }
    if n >= 1_0000_0000_0000_0000u128 {
        return format!("{:.3e}", n as f64);
    }
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gen_keys(pattern: &str) -> Vec<String> {
        generate(pattern, &GenerateOptions::default())
            .unwrap_or_else(|e| panic!("`{pattern}` 生成失败：{e}"))
            .keys
    }

    #[test]
    fn literal_only() {
        assert_eq!(gen_keys(r"sk-abc"), vec!["sk-abc".to_string()]);
    }

    #[test]
    fn anchors_are_ignored() {
        assert_eq!(gen_keys(r"^sk-abc$"), vec!["sk-abc".to_string()]);
    }

    #[test]
    fn char_class_and_quantifier() {
        let v = gen_keys(r"^sk-[0-9]{2}$");
        assert_eq!(v.len(), 100);
        assert_eq!(v[0], "sk-00");
        assert_eq!(v[9], "sk-09");
        assert_eq!(v[10], "sk-10");
        assert_eq!(v[99], "sk-99");
    }

    #[test]
    fn quantifier_range_shortest_first() {
        let v = gen_keys(r"a[0-9]{1,2}");
        // 先长度 1（10 条），再长度 2（100 条）
        assert_eq!(v.len(), 110);
        assert_eq!(v[0], "a0");
        assert_eq!(v[9], "a9");
        assert_eq!(v[10], "a00");
        assert_eq!(v[109], "a99");
    }

    #[test]
    fn alternation() {
        let v = gen_keys(r"sk-(dev|prod)-1");
        assert_eq!(v, vec!["sk-dev-1".to_string(), "sk-prod-1".to_string()]);
    }

    #[test]
    fn non_capturing_group() {
        assert_eq!(gen_keys(r"sk-(?:a|b)"), vec!["sk-a".to_string(), "sk-b".to_string()]);
    }

    #[test]
    fn escapes() {
        assert_eq!(gen_keys(r"\d\d").len(), 100);
        assert_eq!(gen_keys(r"\w").len(), 63);
        // \D 是可打印 ASCII 去掉数字
        assert_eq!(gen_keys(r"\D").len(), 95 - 10);
    }

    #[test]
    fn whitespace_drops_control_characters() {
        // \s 含 \t \n \r \v \f 五个控制字符，它们无法作为 HTTP 头的值，会被剔除，
        // 只剩空格本身
        let dict = generate(r"\s", &GenerateOptions::default()).unwrap();
        assert_eq!(dict.keys, vec![" ".to_string()]);
        assert_eq!(dict.dropped, 5);
    }

    #[test]
    fn dot_is_printable_ascii() {
        assert_eq!(gen_keys(r".").len(), 95);
    }

    #[test]
    fn negated_class() {
        // 全集 95 个字符，排除 a/b/c
        assert_eq!(gen_keys(r"[^abc]").len(), 92);
    }

    #[test]
    fn escaped_meta_characters() {
        assert_eq!(gen_keys(r"a\.b"), vec!["a.b".to_string()]);
        assert_eq!(gen_keys(r"a\+b"), vec!["a+b".to_string()]);
    }

    #[test]
    fn unbounded_star_uses_option() {
        let dict = generate(
            r"x*",
            &GenerateOptions {
                max_results: 1000,
                unbounded_repeat: 3,
                ..GenerateOptions::default()
            },
        )
        .unwrap();
        // 0..=3 次重复："" "x" "xx" "xxx"
        assert_eq!(dict.keys, vec!["", "x", "xx", "xxx"]);
    }

    #[test]
    fn literal_run_followed_by_quantifier() {
        // `sk-` 是字面串，量词只作用于紧邻的前一个字符（`-`），
        // 与标准正则语义一致
        let v = gen_keys(r"sk-a{2}");
        assert_eq!(v, vec!["sk-aa".to_string()]);
    }

    #[test]
    fn recommended_max_keys_tracks_available_memory() {
        // 32 位 hex：单条上界 = 3 字面前缀 + 32 + 64 开销 = 99 字节
        let keys = recommended_max_keys(r"sk-[a-f0-9]{32}", 3).unwrap();
        let avail = available_memory_bytes().unwrap();
        assert_eq!(keys, avail / 99, "推荐条数 = 可用运存 ÷ 单条字节上界");
        // 非法正则 → 无法预估（None）
        assert!(recommended_max_keys(r"sk-[a-f0-9]{32}((", 3).is_none());
        // 合法但空间巨大 → 推荐条数与正则长度线性相关
        let long = recommended_max_keys(r"sk-[A-Za-z0-9]{64}", 3).unwrap();
        assert!(long < keys);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn release_unused_memory_returns_rss_after_big_dict() {
        // 验证：生成 100 万条字典 → drop → malloc_trim 后 RSS 明显回落
        fn rss_mb() -> u64 {
            let text = std::fs::read_to_string("/proc/self/status").unwrap();
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("VmRSS:") {
                    return rest.trim().trim_end_matches("kB").trim().parse().unwrap();
                }
            }
            0
        }
        let opts = GenerateOptions {
            max_results: 1_000_000,
            random_sample: true,
            seed: 7,
            ..GenerateOptions::default()
        };
        let dict = generate(r"sk-[a-f0-9]{12}", &opts).unwrap();
        assert_eq!(dict.keys.len(), 1_000_000);
        let peak = rss_mb();
        drop(dict);
        crate::utils::sniffer::release_unused_memory();
        let after = rss_mb();
        // 1M 条字典约 80-100 MB；trim 后应明显回落（宽松断言 ≥ 30 MB 差）
        assert!(
            peak.saturating_sub(after) >= 30,
            "peak={peak} MB, after={after} MB（回落不足）"
        );
    }

    #[test]
    fn huge_max_results_refused_instead_of_oom() {
        // 复现崩溃场景：150 万亿的「最大生成条数」曾导致一次性申请
        // 1.5e14 字节内存并把进程 OOM 中止；现在应在分配前按可用运存报错。
        let opts = GenerateOptions {
            max_results: 150_000_000_000_000,
            random_sample: true,
            seed: 1,
            ..GenerateOptions::default()
        };
        let err = generate(r"sk-[a-f0-9]{32}", &opts).unwrap_err();
        assert!(err.contains("运存"), "应提示内存不足：{err}");

        // 全量枚举路径（12 位数字空间 ≈ 1 万亿条，任何机器都装不下）同样在分配前被拦下
        let err2 = generate(r"sk-[0-9]{12}", &opts).unwrap_err();
        assert!(err2.contains("运存"), "枚举路径也应提示：{err2}");

        // 正常量级不受影响（不因加了闸门而误伤）
        let ok = generate(
            r"sk-[a-f0-9]{32}",
            &GenerateOptions {
                max_results: 100_000,
                random_sample: true,
                seed: 1,
                ..GenerateOptions::default()
            },
        )
        .unwrap();
        assert_eq!(ok.keys.len(), 100_000);
    }

    #[test]
    fn max_results_caps_output() {
        let dict = generate(
            r"sk-[0-9]{6}",
            &GenerateOptions {
                max_results: 50,
                unbounded_repeat: 1,
                ..GenerateOptions::default()
            },
        )
        .unwrap();
        assert_eq!(dict.keys.len(), 50);
        assert!(dict.truncated);
        assert_eq!(dict.total_space, 1_000_000);
        assert_eq!(dict.keys[0], "sk-000000");
    }

    #[test]
    fn control_chars_are_dropped() {
        let dict = generate(r"a\tb", &GenerateOptions::default()).unwrap();
        assert_eq!(dict.dropped, 1);
        assert!(dict.keys.is_empty());
    }

    #[test]
    fn bad_patterns_error() {
        assert!(generate(r"sk-[0-9", &GenerateOptions::default()).is_err());
        assert!(generate(r"*abc", &GenerateOptions::default()).is_err());
        assert!(generate(r"a)", &GenerateOptions::default()).is_err());
        assert!(generate(r"a{5,2}", &GenerateOptions::default()).is_err());
    }

    #[test]
    fn space_estimation() {
        assert_eq!(estimate_space(r"sk-[0-9]{6}", 3).unwrap(), 1_000_000);
        assert_eq!(estimate_space(r"sk-[A-Za-z0-9]{48}", 3).unwrap() >= 1u128 << 100, true);
    }

    #[test]
    fn format_count_readable() {
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1000), "1,000");
        assert_eq!(format_count(1_000_000), "1,000,000");
        assert!(format_count(u128::MAX).contains("超出"));
    }

    #[test]
    fn braces_without_quantifier_is_literal() {
        assert_eq!(gen_keys(r"a{b}"), vec!["a{b}".to_string()]);
    }

    #[test]
    fn shuffled_is_a_permutation_and_seed_stable() {
        // 小空间（10_000 ≤ 上限×4）：完整枚举 + 种子洗牌，元素是原集的一个排列
        let base = generate(
            r"sk-[0-9]{4}",
            &GenerateOptions {
                max_results: 10_000,
                ..GenerateOptions::default()
            },
        )
        .unwrap();
        let shuffled = |seed: u64| {
            generate(
                r"sk-[0-9]{4}",
                &GenerateOptions {
                    max_results: 10_000,
                    random_sample: true,
                    seed,
                    ..GenerateOptions::default()
                },
            )
            .unwrap()
        };
        let a = shuffled(42);
        let b = shuffled(42);
        let c = shuffled(7);
        // 同一种子 → 完全相同的乱序
        assert_eq!(a.keys, b.keys);
        // 不同种子 → 顺序不同（空间足够大，几乎必然不同）
        assert_ne!(a.keys, c.keys);
        // 乱序是原集合的一个排列（元素完全相同）
        let mut s1 = a.keys.clone();
        let mut s2 = base.keys.clone();
        s1.sort();
        s2.sort();
        assert_eq!(s1, s2);
        // 无重复
        let uniq: std::collections::HashSet<&String> = a.keys.iter().collect();
        assert_eq!(uniq.len(), a.keys.len());
    }

    #[test]
    fn random_sample_is_uniform_across_high_digits() {
        // 大空间（16^32）：按位独立均匀采样 → 最高位不能再全是 0
        let dict = generate(
            r"sk-[a-f0-9]{32}",
            &GenerateOptions {
                max_results: 4000,
                random_sample: true,
                seed: 42,
                ..GenerateOptions::default()
            },
        )
        .unwrap();
        assert_eq!(dict.keys.len(), 4000);
        for k in &dict.keys {
            assert_eq!(k.len(), 35);
            assert!(k.starts_with("sk-"));
            assert!(k[3..]
                .chars()
                .all(|c| matches!(c, '0'..='9' | 'a'..='f')));
        }
        // 最高位在 4000 个样本里应几乎必然覆盖全部 16 个字符；宽松断言 ≥ 8 个
        let first: std::collections::HashSet<char> = dict
            .keys
            .iter()
            .filter_map(|k| k.chars().nth(3))
            .collect();
        assert!(first.len() >= 8, "高位分布应均匀，实际仅有 {first:?}");
        // 同一种子 → 完全相同（断点续跑依赖这个）
        let again = generate(
            r"sk-[a-f0-9]{32}",
            &GenerateOptions {
                max_results: 4000,
                random_sample: true,
                seed: 42,
                ..GenerateOptions::default()
            },
        )
        .unwrap();
        assert_eq!(dict.keys, again.keys);
        // 不同种子 → 内容不同（几乎必然）
        let other = generate(
            r"sk-[a-f0-9]{32}",
            &GenerateOptions {
                max_results: 4000,
                random_sample: true,
                seed: 7,
                ..GenerateOptions::default()
            },
        )
        .unwrap();
        assert_ne!(dict.keys, other.keys);
    }

    #[test]
    fn parallel_sample_mid_space_is_dispersed_and_deterministic() {
        // 中空间（1,000,000 > 上限 100,000）：走多线程并行采样路径（16 线程）
        let dict = generate(
            r"sk-[0-9]{6}",
            &GenerateOptions {
                max_results: 100_000,
                random_sample: true,
                seed: 2026,
                ..GenerateOptions::default()
            },
        )
        .unwrap();
        assert_eq!(dict.keys.len(), 100_000);
        // 无重复（排序去重 + 补采保证）
        let uniq: std::collections::HashSet<&String> = dict.keys.iter().collect();
        assert_eq!(uniq.len(), dict.keys.len());
        // 第一位（十万位上的数字）不应只出现 0-5 这类前缀 —— 应覆盖全部 10 个数字
        let first: std::collections::HashSet<char> = dict
            .keys
            .iter()
            .filter_map(|k| k.chars().nth(3))
            .collect();
        assert_eq!(first.len(), 10, "十万位分布应覆盖全部数字，实际 {first:?}");
        // 同一种子 → 完全相同（多线程结果也必须可复现）
        let again = generate(
            r"sk-[0-9]{6}",
            &GenerateOptions {
                max_results: 100_000,
                random_sample: true,
                seed: 2026,
                ..GenerateOptions::default()
            },
        )
        .unwrap();
        assert_eq!(dict.keys, again.keys);
    }

    #[test]
    fn small_space_within_limit_is_complete() {
        // 空间（1000）≤ 上限（100,000）：完整枚举 + 种子洗牌，一个不漏且乱序
        let base = generate(
            r"sk-[0-9]{3}",
            &GenerateOptions {
                max_results: 100_000,
                ..GenerateOptions::default()
            },
        )
        .unwrap();
        let shuf = generate(
            r"sk-[0-9]{3}",
            &GenerateOptions {
                max_results: 100_000,
                random_sample: true,
                seed: 9,
                ..GenerateOptions::default()
            },
        )
        .unwrap();
        assert_eq!(base.keys.len(), 1000);
        assert_eq!(shuf.keys.len(), 1000);
        assert!(!shuf.truncated);
        // 内容完全一致（完整覆盖），只是顺序不同
        let mut a = base.keys.clone();
        let mut b = shuf.keys.clone();
        a.sort();
        b.sort();
        assert_eq!(a, b);
        assert_ne!(base.keys, shuf.keys);
    }
}
