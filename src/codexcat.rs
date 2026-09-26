use serde::Serialize;

const CODEX_PROMPT: &str = include_str!("../internal/codexcat/codex_prompt.md");

#[derive(Serialize)]
pub(crate) struct Catalog {
    models: Vec<CatalogModel>,
}

#[derive(Serialize)]
struct CatalogModel {
    slug: String,
    display_name: String,
    description: String,
    base_instructions: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_reasoning_level: Option<String>,
    supported_reasoning_levels: Vec<ReasoningLevel>,
    shell_type: &'static str,
    visibility: &'static str,
    supported_in_api: bool,
    priority: usize,
    support_verbosity: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_verbosity: Option<&'static str>,
    apply_patch_tool_type: &'static str,
    truncation_policy: TruncationPolicy,
    experimental_supported_tools: Vec<String>,
    input_modalities: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_window: Option<usize>,
}

#[derive(Serialize)]
struct ReasoningLevel {
    effort: String,
    description: &'static str,
}

#[derive(Serialize)]
struct TruncationPolicy {
    mode: &'static str,
    limit: usize,
}

pub(crate) fn render() -> anyhow::Result<Catalog> {
    let (groups, entries) = crate::provider::desktop_group_data()?;
    let mut models = Vec::with_capacity(groups.len() + entries.len());

    for group in groups.into_iter().filter(|group| !group.hidden) {
        let members = group
            .members
            .iter()
            .filter_map(|member| entries.iter().find(|entry| entry.id == *member))
            .collect::<Vec<_>>();
        if members.is_empty() {
            continue;
        }

        let efforts = members
            .iter()
            .flat_map(|member| member.model.efforts.iter().cloned())
            .fold(Vec::new(), |mut efforts, effort| {
                if !efforts.contains(&effort) {
                    efforts.push(effort);
                }
                efforts
            });
        let context_window = members
            .iter()
            .map(|member| member.model.context)
            .filter(|context| *context > 0)
            .min();
        let images = members.iter().all(|member| member.model.images);

        let priority = models.len() + 1;
        models.push(model(
            format!("group/{}", group.id),
            format!("{} · routing group", group.name),
            efforts,
            images,
            context_window,
            priority,
        ));
    }

    for entry in entries {
        let crate::provider::ModelEntry {
            id,
            model: model_data,
            provider_name,
            ..
        } = entry;
        let name = if model_data.name.is_empty() {
            model_data.id.as_str()
        } else {
            model_data.name.as_str()
        };
        let priority = models.len() + 1;
        models.push(model(
            id,
            format!("{name} · {provider_name}"),
            model_data.efforts,
            model_data.images,
            (model_data.context > 0).then_some(model_data.context),
            priority,
        ));
    }

    Ok(Catalog { models })
}

fn model(
    slug: String,
    display_name: String,
    efforts: Vec<String>,
    images: bool,
    context_window: Option<usize>,
    priority: usize,
) -> CatalogModel {
    let default_reasoning_level = default_effort(&efforts);
    CatalogModel {
        description: format!("{display_name} via magpie"),
        slug,
        display_name,
        base_instructions: CODEX_PROMPT,
        default_reasoning_level,
        supported_reasoning_levels: efforts
            .into_iter()
            .map(|effort| ReasoningLevel {
                effort,
                description: "",
            })
            .collect(),
        shell_type: "unified_exec",
        visibility: "list",
        supported_in_api: true,
        priority,
        support_verbosity: true,
        default_verbosity: None,
        apply_patch_tool_type: "freeform",
        truncation_policy: TruncationPolicy {
            mode: "tokens",
            limit: 10_000,
        },
        experimental_supported_tools: Vec::new(),
        input_modalities: if images {
            vec!["text", "image"]
        } else {
            vec!["text"]
        },
        context_window,
    }
}

pub(crate) fn default_effort(efforts: &[String]) -> Option<String> {
    ["medium", "high"]
        .into_iter()
        .find(|preferred| efforts.iter().any(|effort| effort == preferred))
        .map(str::to_owned)
        .or_else(|| efforts.first().cloned())
}
