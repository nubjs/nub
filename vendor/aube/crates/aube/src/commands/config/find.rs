use super::{literal_aliases, setting_search_score, settings_meta};
use clap::Args;

#[derive(Debug, Args)]
pub struct FindArgs {
    /// Words to search for.
    #[arg(required = true)]
    pub query: Vec<String>,
}

pub fn run(args: FindArgs) -> miette::Result<()> {
    let terms = args
        .query
        .iter()
        .map(|q| q.to_ascii_lowercase())
        .collect::<Vec<_>>();

    let mut matches = settings_meta::all()
        .iter()
        .filter_map(|meta| {
            let score = setting_search_score(meta, &terms);
            (score > 0).then_some((score, *meta))
        })
        .collect::<Vec<_>>();

    matches.sort_by(|(a_score, a), (b_score, b)| {
        b_score.cmp(a_score).then_with(|| a.name.cmp(b.name))
    });

    if matches.is_empty() {
        println!("No settings matched `{}`", args.query.join(" "));
        return Ok(());
    }

    for (_, meta) in matches.into_iter().take(20) {
        let key = literal_aliases(meta.npmrc_keys)
            .into_iter()
            .next()
            .unwrap_or_else(|| meta.name.to_string());
        println!("{} ({}) - {}", meta.name, key, meta.description);
    }

    Ok(())
}
