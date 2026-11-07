//! `appstore-connect-mcp toolsets` — наборы инструментов и число инструментов в них по
//! уровням доступа.
//!
//! Считаются инструменты операций и составные инструменты набора; `api_search` и
//! `api_request` регистрируются всегда и в наборы не входят. Ни профиль, ни ключ, ни сеть не
//! нужны: таблица операций встроена в бинарник.

use appstore_connect_api::AccessLevel;
use appstore_connect_api::spec::operations;
use appstore_connect_mcp_core::{CompositeTool, available_toolsets, composite};
use clap::Args;

#[derive(Debug, Args)]
#[command(after_help = "\
READ, WRITE and DESTRUCTIVE count the tools of each access level (operation tools and \
composite tools). A read-only server registers the READ tools of its toolsets, --allow-write \
adds the WRITE tools and --allow-destructive the DESTRUCTIVE ones. `api_search` and \
`api_request` are always registered and belong to no toolset. DEFAULT marks the toolsets \
enabled without --toolsets.")]
pub struct ToolsetsArgs {}

pub fn run(_args: ToolsetsArgs) -> anyhow::Result<()> {
    print!("{}", render(&composite::all()));
    Ok(())
}

/// Число инструментов набора по уровням доступа: чтение, запись, удаление.
fn counts(slug: &str, composite: &[CompositeTool]) -> [usize; 3] {
    let mut counts = [0; 3];
    let levels = operations()
        .iter()
        .filter(|op| op.toolset == slug)
        .map(|op| op.access)
        .chain(
            composite
                .iter()
                .filter(|tool| tool.toolset == slug)
                .map(|tool| tool.access),
        );
    for access in levels {
        let index = match access {
            AccessLevel::Read => 0,
            AccessLevel::Write => 1,
            AccessLevel::Destructive => 2,
        };
        counts[index] += 1;
    }
    counts
}

fn render(composite: &[CompositeTool]) -> String {
    let toolsets = available_toolsets();
    let width = toolsets
        .iter()
        .map(|t| t.slug.len())
        .max()
        .unwrap_or(0)
        .max("TOOLSET".len());
    let mut out = format!(
        "{:<width$}  {:>4}  {:>5}  {:>11}  {:<7}  DESCRIPTION\n",
        "TOOLSET", "READ", "WRITE", "DESTRUCTIVE", "DEFAULT"
    );
    let mut total = [0; 3];
    for toolset in &toolsets {
        let [read, write, destructive] = counts(toolset.slug, composite);
        total[0] += read;
        total[1] += write;
        total[2] += destructive;
        out.push_str(&format!(
            "{:<width$}  {read:>4}  {write:>5}  {destructive:>11}  {:<7}  {}\n",
            toolset.slug,
            if toolset.default { "yes" } else { "" },
            toolset.description
        ));
    }
    out.push_str(&format!(
        "{:<width$}  {:>4}  {:>5}  {:>11}\n",
        "total", total[0], total[1], total[2]
    ));
    out
}

#[cfg(test)]
mod tests {
    use appstore_connect_mcp_core::DEFAULT_TOOLSETS;

    use super::*;

    #[test]
    fn counts_cover_every_operation_and_composite_tool() {
        let composite = composite::all();
        let total: usize = available_toolsets()
            .iter()
            .map(|t| counts(t.slug, &composite).iter().sum::<usize>())
            .sum();
        assert_eq!(total, operations().len() + composite.len());

        let text = render(&composite);
        let mut lines = text.lines();
        let header = lines.next().unwrap();
        assert!(
            header.starts_with("TOOLSET") && header.contains("READ  WRITE  DESTRUCTIVE  DEFAULT"),
            "{header}"
        );
        for slug in DEFAULT_TOOLSETS {
            let line = text
                .lines()
                .find(|line| line.split_whitespace().next() == Some(*slug))
                .unwrap();
            assert!(line.contains(" yes "), "{line}");
        }
        assert!(text.lines().any(|line| line.starts_with("total")), "{text}");
    }
}
