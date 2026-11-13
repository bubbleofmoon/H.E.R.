//! Briefing assembly over the four Memory layers (GLOSSARY "Memory layers"), each with a budget.
//!
//! DSH briefing sections belong to Identity, Relationship or Conversation; the wake delta's automatic
//! recall is Material. Budgets are runtime configuration (`memory.layers`, read every wake) in estimated
//! tokens. Identity and Relationship are never cut: what they use beyond their own budgets comes out of
//! Material first, then Conversation. Conversation is cut to what is left from its last section backwards,
//! oldest line first; Material keeps the leading (best ranked) recalled items that fit. Sections outside the
//! layers (behaviour, tool and sending rules) are never cut or counted.

use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::sync::{Mutex, OnceLock};

use crate::runtime::RuntimeContext;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer { Identity, Relationship, Conversation, Material }

impl Layer {
    pub const ALL: [Layer; 4] = [Layer::Identity, Layer::Relationship, Layer::Conversation, Layer::Material];
    pub fn name(self) -> &'static str {
        match self {
            Layer::Identity => "identity",
            Layer::Relationship => "relationship",
            Layer::Conversation => "conversation",
            Layer::Material => "material",
        }
    }
}

/// `memory.layers.*`: estimated tokens per layer and the most recalled items Material carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budgets {
    pub identity: usize,
    pub relationship: usize,
    pub conversation: usize,
    pub material: usize,
    pub material_items: usize,
}

impl Budgets {
    /// Out-of-range or non-integer values use the starting defaults (1500/1500/800/1000 tokens, 5 items).
    pub fn from_cfg(cfg: &Value) -> Self {
        let read = |name: &str, default: usize, max: u64| {
            cfg.pointer(&format!("/memory/layers/{name}"))
                .and_then(Value::as_u64)
                .filter(|n| (1..=max).contains(n))
                .map_or(default, |n| n as usize)
        };
        Budgets {
            identity: read("identityTokens", 1500, 100_000),
            relationship: read("relationshipTokens", 1500, 100_000),
            conversation: read("conversationTokens", 800, 100_000),
            material: read("materialTokens", 1000, 100_000),
            material_items: read("materialItems", 5, 20),
        }
    }
}

/// Estimated model tokens: one per non-ASCII character (Han text), one per four ASCII characters.
pub fn estimate_tokens(text: &str) -> usize {
    let ascii = text.chars().filter(|c| c.is_ascii()).count();
    text.chars().count() - ascii + (ascii + 3) / 4
}

/// Items per layer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub identity: usize,
    pub relationship: usize,
    pub conversation: usize,
    pub material: usize,
}

impl Counts {
    pub fn get(&self, layer: Layer) -> usize {
        match layer {
            Layer::Identity => self.identity,
            Layer::Relationship => self.relationship,
            Layer::Conversation => self.conversation,
            Layer::Material => self.material,
        }
    }
    fn add(&mut self, layer: Layer, n: usize) {
        match layer {
            Layer::Identity => self.identity += n,
            Layer::Relationship => self.relationship += n,
            Layer::Conversation => self.conversation += n,
            Layer::Material => self.material += n,
        }
    }
    pub fn to_json(&self) -> Value {
        Value::Object(Layer::ALL.iter().map(|l| (l.name().to_string(), json!(self.get(*l)))).collect())
    }
}

enum Body {
    /// Rendered back to back; each non-empty part is one item.
    Parts(Vec<String>),
    /// `(heading, lines)` groups (`render_groups`); each line is one item.
    Groups(Vec<(String, Vec<String>)>),
}

/// One briefing section and the layer it belongs to (`None`: a rule section, never cut or counted).
pub struct Section {
    name: &'static str,
    layer: Option<Layer>,
    body: Body,
}

impl Section {
    pub fn parts(name: &'static str, layer: Option<Layer>, parts: Vec<String>) -> Self {
        Section { name, layer, body: Body::Parts(parts) }
    }
    pub fn groups(name: &'static str, layer: Option<Layer>, groups: Vec<(String, Vec<String>)>) -> Self {
        Section { name, layer, body: Body::Groups(groups) }
    }
    fn render(&self) -> String {
        match &self.body {
            Body::Parts(parts) => parts.concat(),
            Body::Groups(groups) => render_groups(groups),
        }
    }
    fn items(&self) -> usize {
        match &self.body {
            Body::Parts(parts) => parts.iter().filter(|p| !p.is_empty()).count(),
            Body::Groups(groups) => groups.iter().map(|(_, lines)| lines.len()).sum(),
        }
    }
    /// Remove one item: the last non-empty part, or the first (oldest) line of the last non-empty group.
    fn cut_one(&mut self) -> bool {
        match &mut self.body {
            Body::Parts(parts) => match parts.iter_mut().rev().find(|p| !p.is_empty()) {
                Some(part) => { part.clear(); true }
                None => false,
            },
            Body::Groups(groups) => match groups.iter_mut().rev().find(|(_, lines)| !lines.is_empty()) {
                Some((_, lines)) => { lines.remove(0); true }
                None => false,
            },
        }
    }
}

/// Non-empty groups as `heading\nline\nline`, joined by a newline and closed by a blank line; nothing when empty.
pub fn render_groups(groups: &[(String, Vec<String>)]) -> String {
    let parts: Vec<String> = groups.iter().filter(|(_, lines)| !lines.is_empty())
        .map(|(head, lines)| std::iter::once(head.as_str()).chain(lines.iter().map(String::as_str)).collect::<Vec<_>>().join("\n"))
        .collect();
    if parts.is_empty() { String::new() } else { format!("{}\n\n", parts.join("\n")) }
}

/// The briefing sections after the budgets, with the items each layer injects and lost.
pub struct Assembly {
    pub sections: BTreeMap<&'static str, String>,
    /// Material is filled in by the caller once recall has run (`material_fit`).
    pub counts: Counts,
    pub cut: Counts,
    /// What Identity and Relationship left of Material's budget.
    pub material_tokens: usize,
    pub material_items: usize,
}

impl Assembly {
    /// How many leading recalled items (best ranked first) fit Material's remaining budget and item cap.
    pub fn material_fit(&self, items: &[String]) -> usize {
        let mut used = 0;
        items.iter().take(self.material_items)
            .take_while(|item| { used += estimate_tokens(item); used <= self.material_tokens })
            .count()
    }
}

pub fn assemble(mut sections: Vec<Section>, budgets: &Budgets) -> Assembly {
    let tokens = |sections: &[Section], layer: Layer| -> usize {
        sections.iter().filter(|s| s.layer == Some(layer)).map(|s| estimate_tokens(&s.render())).sum()
    };
    let over = tokens(&sections, Layer::Identity).saturating_sub(budgets.identity)
        + tokens(&sections, Layer::Relationship).saturating_sub(budgets.relationship);
    let from_material = over.min(budgets.material);
    let conversation_budget = budgets.conversation.saturating_sub(over - from_material);
    let mut cut = Counts::default();
    let order: Vec<usize> = (0..sections.len()).rev()
        .filter(|&i| sections[i].layer == Some(Layer::Conversation)).collect();
    for i in order {
        while tokens(&sections, Layer::Conversation) > conversation_budget && sections[i].cut_one() {
            cut.conversation += 1;
        }
    }
    let mut counts = Counts::default();
    for section in &sections {
        if let Some(layer) = section.layer { counts.add(layer, section.items()); }
    }
    Assembly {
        sections: sections.iter().map(|s| (s.name, s.render())).collect(),
        counts,
        cut,
        material_tokens: budgets.material - from_material,
        material_items: budgets.material_items,
    }
}

/// The active persona core for `key` (Identity). Seam for #19: the persona library supplies this text
/// (`persona::active_core(context, key)`); until then there is none.
pub fn persona_core(_context: &RuntimeContext, _key: &str) -> String {
    String::new()
}

/// Identity is checked in every conversation; Relationship only in the owner's private chat until impression
/// cards exist (#26).
fn empty_protected(counts: &Counts, key: &str, cfg: &Value) -> Vec<Layer> {
    let mut empty = Vec::new();
    if counts.identity == 0 { empty.push(Layer::Identity); }
    if counts.relationship == 0 && crate::conversation_authority::owner_conversation(cfg).as_deref() == Some(key) {
        empty.push(Layer::Relationship);
    }
    empty
}

/// Alert the owner when a protected layer injected nothing, once per conversation, session generation and
/// layer in this process. Returns the layers alerted now.
pub fn alert_empty(context: &RuntimeContext, cfg: &Value, key: &str, generation: u64, counts: &Counts) -> Vec<Layer> {
    let scope = context.runtime_root().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    alert_empty_with(&scope, cfg, key, generation, counts, &|text, dedupe| crate::admin_alert::notify_memory(context, cfg, text, dedupe))
}

fn alert_empty_with(scope: &str, cfg: &Value, key: &str, generation: u64, counts: &Counts,
    notify: &dyn Fn(&str, &str) -> bool) -> Vec<Layer> {
    static RAISED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let mut raised = RAISED.get_or_init(Default::default).lock().unwrap_or_else(|p| p.into_inner());
    let mut alerted = Vec::new();
    for layer in empty_protected(counts, key, cfg) {
        if raised.len() >= 4096 { raised.clear(); }
        if raised.insert(format!("{scope}\n{key}\n{generation}\n{}", layer.name())) {
            let (label, meaning) = if layer == Layer::Identity { ("身份", "自己是谁") } else { ("关系", "在和谁说话") };
            notify(&format!("会话 {key} 的简报没有注入任何{label}层内容，她可能不知道{meaning}。"),
                &format!("memory-layer-{}-{key}", layer.name()));
            alerted.push(layer);
        }
    }
    alerted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> Budgets { Budgets::from_cfg(&json!({})) }

    fn layered() -> Vec<Section> {
        vec![
            Section::parts("identity", Some(Layer::Identity), vec!["【你的身份】小鲸鱼\n\n".into(), String::new()]),
            Section::parts("operator", Some(Layer::Relationship), vec!["背景\n\n".into()]),
            Section::groups("impressions", Some(Layer::Relationship), vec![("【对群友的印象】".into(), vec!["- 甲：爱猫".into(), "- 乙：爱狗".into()])]),
            Section::groups("memory", Some(Layer::Conversation), vec![
                ("【进行中的话题】".into(), vec!["- 旧话题".into(), "- 新话题".into()]),
                ("【你想说但还没说的】".into(), vec!["- 旧想法".into(), "- 新想法".into()])]),
            Section::parts("participation", Some(Layer::Conversation), vec!["参与度\n\n".into()]),
            Section::parts("behaviour", None, vec!["行为规则\n\n".into()]),
        ]
    }

    #[test]
    fn within_budget_every_section_renders_as_given_and_counts_items_per_layer() {
        let a = assemble(layered(), &defaults());
        assert_eq!(a.sections["identity"], "【你的身份】小鲸鱼\n\n");
        assert_eq!(a.sections["impressions"], "【对群友的印象】\n- 甲：爱猫\n- 乙：爱狗\n\n");
        assert_eq!(a.sections["memory"], "【进行中的话题】\n- 旧话题\n- 新话题\n【你想说但还没说的】\n- 旧想法\n- 新想法\n\n");
        assert_eq!(a.sections["behaviour"], "行为规则\n\n");
        assert_eq!(a.counts, Counts { identity: 1, relationship: 3, conversation: 5, material: 0 });
        assert_eq!(a.cut, Counts::default());
        assert_eq!((a.material_tokens, a.material_items), (1000, 5));
    }

    #[test]
    fn identity_and_relationship_overflow_comes_out_of_material_first_then_conversation() {
        let full = assemble(layered(), &defaults());
        let identity = estimate_tokens(&full.sections["identity"]);
        let conversation = estimate_tokens(&full.sections["memory"]) + estimate_tokens(&full.sections["participation"]);
        // Identity is 5 over its budget; Material holds 3 of it, Conversation gives up the other 2.
        let a = assemble(layered(), &Budgets { identity: identity - 5, conversation, material: 3, ..defaults() });
        assert_eq!(a.sections["identity"], "【你的身份】小鲸鱼\n\n", "identity is never cut");
        assert_eq!(a.material_tokens, 0);
        assert_eq!(a.sections["participation"], "", "conversation loses its last section first");
        assert_eq!(a.sections["memory"], full.sections["memory"]);
        assert_eq!(a.cut.conversation, 1);
        assert_eq!(a.counts.relationship, 3);
    }

    #[test]
    fn conversation_is_cut_from_the_last_section_backwards_and_oldest_line_first() {
        let kept = "【进行中的话题】\n- 旧话题\n- 新话题\n【你想说但还没说的】\n- 新想法\n\n";
        let a = assemble(layered(), &Budgets { conversation: estimate_tokens(kept), ..defaults() });
        assert_eq!(a.sections["participation"], "");
        assert_eq!(a.sections["memory"], kept);
        assert_eq!((a.cut.conversation, a.counts.conversation), (2, 3));
        let a = assemble(layered(), &Budgets { conversation: 1, ..defaults() });
        assert_eq!(a.sections["memory"], "");
        assert_eq!((a.cut.conversation, a.counts.conversation), (5, 0));
        assert_eq!(a.sections["impressions"], "【对群友的印象】\n- 甲：爱猫\n- 乙：爱狗\n\n", "relationship is never cut");
    }

    #[test]
    fn material_keeps_the_leading_items_that_fit_its_remaining_budget_and_item_cap() {
        let items: Vec<String> = ["- [fact] 第一条", "- [fact] 第二条", "- [fact] 第三条"].map(String::from).to_vec();
        let one = estimate_tokens(&items[0]);
        let mut a = assemble(Vec::new(), &Budgets { material: one * 2, ..defaults() });
        assert_eq!(a.material_fit(&items), 2);
        a.material_items = 1;
        assert_eq!(a.material_fit(&items), 1);
        a.material_tokens = 0;
        assert_eq!(a.material_fit(&items), 0);
    }

    #[test]
    fn budgets_are_read_from_runtime_configuration_with_the_starting_defaults() {
        assert_eq!(defaults(), Budgets { identity: 1500, relationship: 1500, conversation: 800, material: 1000, material_items: 5 });
        let cfg = json!({"memory":{"layers":{"identityTokens":900,"relationshipTokens":700,"conversationTokens":300,"materialTokens":200,"materialItems":2}}});
        assert_eq!(Budgets::from_cfg(&cfg), Budgets { identity: 900, relationship: 700, conversation: 300, material: 200, material_items: 2 });
        let bad = json!({"memory":{"layers":{"identityTokens":0,"conversationTokens":"300","materialItems":21}}});
        assert_eq!(Budgets::from_cfg(&bad), defaults());
    }

    #[test]
    fn token_estimate_counts_han_characters_one_each_and_ascii_by_fours() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("小鲸鱼"), 3);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        assert_eq!(estimate_tokens("小a"), 2);
    }

    #[test]
    fn an_empty_identity_alerts_once_per_generation_and_relationship_only_in_the_owner_chat() {
        let sent = std::cell::RefCell::new(Vec::<String>::new());
        let notify = |text: &str, key: &str| { sent.borrow_mut().push(format!("{key}|{text}")); true };
        let cfg = json!({"ownerQQ": 8808});
        let empty = Counts::default();
        let scope = "briefing-alert-test";
        assert_eq!(alert_empty_with(scope, &cfg, "group:5", 1, &empty, &notify), vec![Layer::Identity], "relationship is not checked in a group");
        assert_eq!(alert_empty_with(scope, &cfg, "group:5", 1, &empty, &notify), Vec::<Layer>::new(), "once per generation");
        assert_eq!(alert_empty_with(scope, &cfg, "group:5", 2, &empty, &notify), vec![Layer::Identity], "a new generation alerts again");
        assert_eq!(alert_empty_with(scope, &cfg, "private:8808", 1, &empty, &notify), vec![Layer::Identity, Layer::Relationship]);
        let full = Counts { identity: 1, relationship: 1, conversation: 0, material: 0 };
        assert_eq!(alert_empty_with(scope, &cfg, "private:8808", 3, &full, &notify), Vec::<Layer>::new(), "empty conversation or material never alerts");
        let sent = sent.into_inner();
        assert_eq!(sent.len(), 4, "{sent:?}");
        assert!(sent[0].starts_with("memory-layer-identity-group:5|") && sent[0].contains("身份"), "{sent:?}");
        assert!(sent[3].starts_with("memory-layer-relationship-private:8808|") && sent[3].contains("关系"), "{sent:?}");
    }
}
