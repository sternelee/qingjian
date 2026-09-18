//! 形码码表：按编码查字词（五笔）。
//!
//! 与 [`crate::Dictionary`] 的区别在键：词库按拼音音节序列查，码表按编码本身查，
//! 敲的编码是词的编码的前缀即可命中（`gan` 命中编码 `gant` 的 开发），没有音节也没有切分。
//!
//! 文件格式 TSV：
//!
//! ```text
//! 词\t编码\t词频
//! 开发\tgant\t9000
//! ```
//!
//! `#` 开头为注释行，空行忽略。编码统一按小写存，查询输入也须已小写。

use std::path::Path;

use crate::error::DictionaryError;
use crate::matching::Match;

/// 一条码表词目。
#[derive(Debug, Clone)]
struct Entry {
    /// 全码，小写。
    code: String,

    /// 词。
    text: String,

    /// 静态词频。
    frequency: u32,
}

/// 形码码表。词目按 `(编码, 词频降序)` 排好，同前缀的是一段连续区间。
///
/// `Clone` 是给回放用的：`Engine::set_code_table` 收所有权，而回放要在方案之间来回切，
/// 手里得留一份（整份日志通常只有一套方案，切的次数很少）。
#[derive(Debug, Default, Clone)]
pub struct CodeTable {
    /// 按编码字节序升序，同一个编码下按词频降序。
    entries: Vec<Entry>,

    /// 全部词频之和，上下文得分的兜底用。
    total_frequency: u64,

    /// 数据里最长编码的位数（五笔 86 / 98 都是 4）。万能键那条路靠它早退。
    max_code_len: usize,

    /// 万能键：编码里出现它时该位匹配任意字母。五笔是 `z`；换了方案可以用
    /// [`Self::with_wildcard`] 关掉或换掉。
    wildcard: Option<char>,
}

/// 五笔的万能键。
pub const DEFAULT_WILDCARD: char = 'z';

impl CodeTable {
    pub fn parse(source: &str) -> Result<Self, DictionaryError> {
        let mut entries: Vec<Entry> = Vec::new();
        for (index, raw) in source.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let number = index + 1;
            let mut fields = line.split('\t');
            let text = fields
                .next()
                .filter(|s| !s.is_empty())
                .ok_or(DictionaryError::Line {
                    line: number,
                    reason: "missing word",
                })?;
            let code = fields
                .next()
                .filter(|s| !s.is_empty())
                .ok_or(DictionaryError::Line {
                    line: number,
                    reason: "missing code",
                })?;
            let frequency = fields
                .next()
                .map(|s| s.trim().parse::<u32>())
                .transpose()
                .map_err(|_| DictionaryError::Line {
                    line: number,
                    reason: "frequency is not a non-negative integer",
                })?
                .unwrap_or(1);
            entries.push(Entry {
                code: code.to_ascii_lowercase(),
                text: text.to_owned(),
                frequency,
            });
        }
        entries.sort_by(|a, b| {
            a.code
                .cmp(&b.code)
                .then_with(|| b.frequency.cmp(&a.frequency))
                .then_with(|| a.text.cmp(&b.text))
        });
        // 同一个词记了两遍（不同码表版本合并）留词频高的那条，排完序就是靠前的那条
        entries.dedup_by(|a, b| a.code == b.code && a.text == b.text);
        let total_frequency = entries.iter().map(|e| u64::from(e.frequency)).sum();
        let max_code_len = entries.iter().map(|e| e.code.len()).max().unwrap_or(0);
        tracing::debug!(entries = entries.len(), "码表加载完成");
        Ok(Self {
            entries,
            total_frequency,
            max_code_len,
            wildcard: Some(DEFAULT_WILDCARD),
        })
    }

    /// 换万能键：`None` 关掉。五笔以外的形码（或不想用学习键的人）可以关。
    pub fn with_wildcard(mut self, wildcard: Option<char>) -> Self {
        self.wildcard = wildcard;
        self
    }

    /// 当前万能键；`None` 为关。
    pub fn wildcard(&self) -> Option<char> {
        self.wildcard
    }

    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, DictionaryError> {
        Self::parse(&std::fs::read_to_string(path)?)
    }

    /// 编码正好等于输入，或输入是它的前缀的词，最多 `limit` 条。
    ///
    /// 编码与输入相等（这个词打全了）的排在最前，其余按词频降序；`limit` 之后的不返回。
    /// 单字母输入（`g`，一级简码）的前缀区间有上万条，所以先按 `limit` 线性选出前面一段再排序，
    /// 不是把整个区间排完再截断——`lookup` 在每次按键的路径上。
    ///
    /// 输入里出现万能键（[`Self::wildcard`]，五笔是 `z`）时改走 [`Self::lookup_wildcard`]，
    /// 否则与 [`Self::lookup_plain`] 相同。
    pub fn lookup(&self, code: &str, limit: usize) -> Vec<Match<'_>> {
        if code.is_empty() || limit == 0 {
            return Vec::new();
        }
        if let Some(wildcard) = self.wildcard.filter(|wildcard| code.contains(*wildcard)) {
            return self.lookup_wildcard(code, wildcard, limit);
        }
        self.lookup_plain(code, limit)
    }

    /// 不认万能键的前缀查询：`z` 当普通字母。混输下让位给拼音时用这个。
    pub fn lookup_plain(&self, code: &str, limit: usize) -> Vec<Match<'_>> {
        if code.is_empty() || limit == 0 {
            return Vec::new();
        }
        let start = self.entries.partition_point(|e| e.code.as_str() < code);
        let hits: Vec<&Entry> = self.entries[start..]
            .iter()
            .take_while(|e| e.code.starts_with(code))
            .collect();
        finish(hits, code, limit, false)
    }

    /// 万能键：与输入**等长**、`wildcard` 位接受任意字母的编码。
    ///
    /// 长度相等就是「打全了」，所以命中的都算 `exact`（与普通前缀查询同一个排序位）；
    /// 编码长过数据里最长的编码时不可能命中，直接返回——否则 `zhongguo` 这种会在整份表里白扫一遍。
    fn lookup_wildcard(&self, code: &str, wildcard: char, limit: usize) -> Vec<Match<'_>> {
        if code.len() > self.max_code_len {
            return Vec::new();
        }
        // 第一个万能键之前那几位是确定的，先在排好序的词目里把区间收窄到这段前缀
        let prefix = &code[..code.find(wildcard).unwrap_or(code.len())];
        let start = self.entries.partition_point(|e| e.code.as_str() < prefix);
        let hits: Vec<&Entry> = self.entries[start..]
            .iter()
            .take_while(|e| e.code.starts_with(prefix))
            .filter(|e| matches_wildcard(code, &e.code, wildcard))
            .collect();
        finish(hits, code, limit, true)
    }

    /// 全部词频之和。
    pub fn total_frequency(&self) -> u64 {
        self.total_frequency
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// 排序、截断、转成 [`Match`]。`all_exact` 为真时全部算打全（万能键那条：长度已经对了）。
fn finish<'a>(
    mut hits: Vec<&'a Entry>,
    code: &str,
    limit: usize,
    all_exact: bool,
) -> Vec<Match<'a>> {
    let order = |a: &&Entry, b: &&Entry| {
        (b.code == code)
            .cmp(&(a.code == code))
            .then_with(|| b.frequency.cmp(&a.frequency))
            .then_with(|| a.code.cmp(&b.code))
            .then_with(|| a.text.cmp(&b.text))
    };
    if hits.len() > limit {
        hits.select_nth_unstable_by(limit, order);
        hits.truncate(limit);
    }
    hits.sort_by(order);
    hits.into_iter()
        .map(|entry| Match {
            text: entry.text.as_str(),
            pinyin: entry.code.as_str(),
            frequency: entry.frequency,
            exact: all_exact || entry.code == code,
        })
        .collect()
}

/// `pattern` 与 `code` 等长，且 `wildcard` 位相同或任意。编码只含小写 ASCII，按字节比。
fn matches_wildcard(pattern: &str, code: &str, wildcard: char) -> bool {
    pattern.len() == code.len()
        && pattern
            .bytes()
            .zip(code.bytes())
            .all(|(pattern, code)| pattern == wildcard as u8 || pattern == code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_up_by_prefix_and_prefers_the_exact_code() {
        let table =
            CodeTable::parse("开发\tgant\t900\n开\tga\t5000\n一\tggll\t100000\n个\twhj\t8000\n")
                .unwrap();
        // 打全了的编码排在前缀命中的词前面，哪怕前缀词词频更高
        let hits = table.lookup("ga", 10);
        assert_eq!(
            hits.iter().map(|h| (h.text, h.exact)).collect::<Vec<_>>(),
            [("开", true), ("开发", false)]
        );
        // 没打全的编码只按词频
        let hits = table.lookup("g", 10);
        assert_eq!(
            hits.iter().map(|h| h.text).collect::<Vec<_>>(),
            ["一", "开", "开发"]
        );
        assert!(hits.iter().all(|h| !h.exact));
        // `z` 是万能键，不能用它当「查不到」的哨兵
        assert!(table.lookup("qqq", 10).is_empty());
        assert!(table.lookup("", 10).is_empty());
        assert!(table.lookup("ga", 0).is_empty());
    }

    #[test]
    fn truncates_to_the_limit_keeping_the_best() {
        // 命中条数多于 limit 时先线性选一段再排，结果要与整体排序的前几条一致
        let table =
            CodeTable::parse("甲\tg\t10\n乙\tg\t50\n丙\tg\t30\n丁\tg\t40\n戊\tg\t20\n").unwrap();
        assert_eq!(
            table
                .lookup("g", 3)
                .iter()
                .map(|h| h.text)
                .collect::<Vec<_>>(),
            ["乙", "丁", "丙"]
        );
        assert_eq!(table.lookup("g", 9).len(), 5);
        assert_eq!(table.lookup("g", 5).len(), 5);
    }

    #[test]
    fn keeps_the_highest_frequency_of_a_duplicated_entry() {
        let table = CodeTable::parse("开发\tgant\t900\n开发\tgant\t3000\n").unwrap();
        assert_eq!(table.len(), 1);
        assert_eq!(table.lookup("gant", 10)[0].frequency, 3000);
    }

    #[test]
    fn rejects_lines_without_a_code() {
        assert!(CodeTable::parse("开发\n").is_err());
        assert!(CodeTable::parse("开发\tgant\t不是数字\n").is_err());
    }

    #[test]
    fn normalizes_codes_to_lowercase() {
        let table = CodeTable::parse("开\tGA\t5000\n").unwrap();
        assert_eq!(table.lookup("ga", 10)[0].text, "开");
    }

    #[test]
    fn wildcard_matches_any_letter_at_that_position_only() {
        let table = CodeTable::parse(
            "我\ttrnt\t900\n特\tcfnt\t800\n开\tga\t5000\n个\twhj\t4000\n工\ta\t6000\n力\tk\t700\n",
        )
        .unwrap();
        // 等长：`trnz` 命中 4 码、前三位 trn 的（我），不算编码更长的
        let hits = table.lookup("trnz", 10);
        assert_eq!(hits.iter().map(|h| h.text).collect::<Vec<_>>(), ["我"]);
        assert!(hits.iter().all(|h| h.exact));
        // 多位：`gz` 只命中 2 码、首位 g 的 → `ga`（开）
        let hits = table.lookup("gz", 10);
        assert_eq!(hits.iter().map(|h| h.text).collect::<Vec<_>>(), ["开"]);
        // 全部位都是万能键：`zzz` 就是「任意 3 码的编码」
        let hits = table.lookup("zzz", 10);
        assert_eq!(hits.iter().map(|h| h.text).collect::<Vec<_>>(), ["个"]);
        // 首位就是万能键：`z` 命中全部 1 码的
        let hits = table.lookup("z", 10);
        assert_eq!(
            hits.iter().map(|h| h.text).collect::<Vec<_>>(),
            ["工", "力"]
        );
        // 长过数据里最长的编码：直接空，不白扫整张表
        assert!(table.lookup("zhongguo", 10).is_empty());
    }

    #[test]
    fn plain_lookup_treats_the_wildcard_as_a_letter() {
        let table = CodeTable::parse("我\ttrnt\t900\n开\tga\t5000\n").unwrap();
        // `trnz` 里 `z` 当普通字母，没有以它开头的编码
        assert!(table.lookup_plain("trnz", 10).is_empty());
        // `trn` 还是普通前缀查询
        assert_eq!(table.lookup_plain("trn", 10)[0].text, "我");
    }

    #[test]
    fn wildcard_can_be_turned_off() {
        let table = CodeTable::parse("我\ttrnt\t900\n")
            .unwrap()
            .with_wildcard(None);
        assert_eq!(table.wildcard(), None);
        // `z` 当普通字母：没有以它开头的编码
        assert!(table.lookup("trnz", 10).is_empty());
    }
}
