// Ports of internal/library/library_test.go's skill tests, and of
// ccswitch_test.go's GitHub update.

use super::*;
use crate::library::testing::write;
// linking a skill into an agent's folder needs a unix filesystem
#[cfg(unix)]
use crate::library::testing::{Sandbox, read as read_file};

fn skill_folder(dir: &std::path::Path, name: &str, description: &str) {
    write(
        dir.join("SKILL.md"),
        &format!("---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n"),
    );
    write(dir.join("scripts/run.sh"), "echo hi\n");
}

#[test]
fn test_github_source() {
    for (input, want) in [
        (
            "owner/repo",
            Source {
                kind: "github".into(),
                repo: "owner/repo".into(),
                ..Source::default()
            },
        ),
        (
            "https://github.com/owner/repo",
            Source {
                kind: "github".into(),
                repo: "owner/repo".into(),
                ..Source::default()
            },
        ),
        (
            "github.com/owner/repo.git",
            Source {
                kind: "github".into(),
                repo: "owner/repo".into(),
                ..Source::default()
            },
        ),
        (
            "https://github.com/o/r/tree/v1/skills/pdf",
            Source {
                kind: "github".into(),
                repo: "o/r".into(),
                ref_: "v1".into(),
                path: "skills/pdf".into(),
                ..Source::default()
            },
        ),
        (
            "https://github.com/o/r/blob/main/skills/pdf/SKILL.md",
            Source {
                kind: "github".into(),
                repo: "o/r".into(),
                ref_: "main".into(),
                path: "skills/pdf".into(),
                ..Source::default()
            },
        ),
    ] {
        let got = github_source(input);
        assert_eq!(got.as_ref(), Some(&want), "{input}");
    }
    for input in ["", "not a repo", "https://gitlab.com/o/r", "/abs/path"] {
        assert!(github_source(input).is_none(), "{input} taken for GitHub");
    }
}

#[test]
fn parse_meta_reads_front_matter() {
    let m = parse_meta(b"---\nname: pdf\ndescription: Reads   PDFs\n---\n\n# pdf\n");
    assert_eq!(m.name, "pdf");
    assert_eq!(m.description, "Reads PDFs");
    let m = parse_meta(b"no front matter");
    assert_eq!(m.name, "");
    assert_eq!(m.description, "");
}

#[test]
fn name_for_fits_a_file_name() {
    let m = Meta {
        name: "My Skill!".to_owned(),
        description: String::new(),
    };
    assert_eq!(name_for(Path::new("/x/pdf"), &m), "My-Skill");
    let m = Meta::default();
    assert_eq!(name_for(Path::new("/x/a_b-c"), &m), "a_b-c");
}

// Skills installed from a folder of the user's stay linked to it: editing
// the folder is editing the skill.
#[cfg(unix)]
#[tokio::test]
async fn test_skills_from_folder() {
    let sb = Sandbox::new();
    let src = sb.home.join("src/skills");
    skill_folder(&src.join("pdf"), "pdf", "Read PDFs");
    skill_folder(&src.join("nested/xlsx"), "xlsx", "Sheets");
    let p = probe_skills_at(&sb.store, src.to_str().unwrap())
        .await
        .unwrap();
    let mut paths: Vec<String> = p.candidates.iter().map(|c| c.path.clone()).collect();
    paths.sort();
    assert_eq!(paths, vec!["nested/xlsx", "pdf"]);

    crate::library::skills::install_skills_at(
        &sb.store,
        &sb.homes,
        src.to_str().unwrap(),
        &["pdf".to_owned()],
        &[
            "claude".to_owned(),
            "codex".to_owned(),
            "opencode".to_owned(),
        ],
    )
    .await
    .unwrap();
    for d in [
        ".claude/skills/pdf",
        ".codex/skills/pdf",
        ".config/opencode/skills/pdf",
    ] {
        assert!(sb.home.join(d).join("SKILL.md").exists(), "{d}");
    }
    // editing the folder is editing the skill
    write(
        src.join("pdf/SKILL.md"),
        "---\nname: pdf\ndescription: Changed\n---\n",
    );
    let v = crate::library::read_at(&sb.store, &sb.homes, &[]).unwrap();
    assert_eq!(v.skills.len(), 1);
    assert_eq!(v.skills[0].description, "Changed");
    assert_eq!(v.skills[0].kind, "folder");

    crate::library::skills::skill_agents_at(&sb.store, &sb.homes, "pdf", vec!["claude".to_owned()])
        .unwrap();
    assert!(
        !sb.home.join(".codex/skills/pdf").exists(),
        "codex still has it"
    );

    crate::library::skills::remove_skill_at(&sb.store, &sb.homes, "pdf").unwrap();
    assert!(
        !sb.home.join(".claude/skills/pdf").exists(),
        "claude still has it"
    );
    assert!(
        src.join("pdf/SKILL.md").exists(),
        "the user's folder went with it"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn test_skill_conflict_and_import() {
    let sb = Sandbox::new();
    skill_folder(&sb.home.join(".claude/skills/notes"), "notes", "Mine");
    let ext = sb.home.join("elsewhere/lint");
    skill_folder(&ext, "lint", "Linked");
    std::fs::create_dir_all(sb.home.join(".codex/skills")).unwrap();
    std::os::unix::fs::symlink(&ext, sb.home.join(".codex/skills/lint")).unwrap();
    std::fs::create_dir_all(sb.home.join(".gemini/skills")).unwrap();
    std::os::unix::fs::symlink(&ext, sb.home.join(".gemini/skills/lint")).unwrap();

    let v = crate::library::read_at(&sb.store, &sb.homes, &[]).unwrap();
    let lint = v
        .found_skills
        .iter()
        .find(|f| f.name == "lint")
        .unwrap_or_else(|| panic!("found skills: {:?}", v.found_skills));
    assert_eq!(lint.agents, vec!["codex", "gemini"]);
    assert!(!lint.link.is_empty());

    crate::library::skills::import_skill_at(&sb.store, &sb.homes, "notes").unwrap();
    crate::library::skills::import_skill_at(&sb.store, &sb.homes, "lint").unwrap();
    let meta = std::fs::symlink_metadata(sb.home.join(".claude/skills/notes")).unwrap();
    assert!(
        meta.file_type().is_symlink(),
        "notes isn't linked from the library now"
    );
    assert!(
        ext.join("SKILL.md").exists(),
        "the folder a link pointed to went"
    );
    for d in [".codex/skills/lint", ".gemini/skills/lint"] {
        assert!(
            ours(&sb.home.join(d), "lint", &sb.store),
            "{d} isn't the library's"
        );
    }

    // the user's own by a name the library has: not overwritten
    skill_folder(&sb.home.join(".cursor/skills/notes"), "notes", "Cursor's");
    let r = crate::library::skills::skill_agents_at(
        &sb.store,
        &sb.homes,
        "notes",
        vec!["claude".to_owned(), "cursor".to_owned()],
    )
    .unwrap();
    assert_eq!(r.problems.len(), 1, "problems: {:?}", r.problems);
    assert_eq!(r.problems[0].agent, "cursor");
    assert!(
        read_file(sb.home.join(".cursor/skills/notes/SKILL.md")).contains("Cursor's"),
        "cursor's own was overwritten"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn test_skills_from_github() {
    let sb = Sandbox::new();
    let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let asked_handler = asked.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new().fallback(move |req: axum::extract::Request| {
        let asked = asked_handler.clone();
        async move {
            let path = req.uri().path().to_owned();
            if path.contains("missing") {
                return (axum::http::StatusCode::NOT_FOUND, Vec::new());
            }
            asked.lock().unwrap().push(path);
            let tarball = crate::library::archive::tar_gz_for_tests(&[
                ("owner-repo-abc/README.md", "hi"),
                (
                    "owner-repo-abc/skills/pdf/SKILL.md",
                    "---\nname: pdf\ndescription: PDFs one\n---\n",
                ),
                ("owner-repo-abc/skills/pdf/forms.md", "forms"),
                (
                    "owner-repo-abc/skills/docx/SKILL.md",
                    "---\nname: docx\ndescription: Word\n---\n",
                ),
                (
                    "owner-repo-abc/template/SKILL.md",
                    "---\nname: template\n---\n",
                ),
            ]);
            (axum::http::StatusCode::OK, tarball)
        }
    });
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    crate::library::skills::set_tarball_base(Some(format!("http://{addr}")));

    let missing = probe_skills_at(&sb.store, "owner/missing").await;
    let error = missing.unwrap_err().to_string();
    assert!(error.contains("no repository"), "missing: {error}");

    let input = "https://github.com/owner/repo/tree/main/skills";
    let p = probe_skills_at(&sb.store, input).await.unwrap();
    assert_eq!(p.candidates.len(), 2, "candidates: {:?}", p.candidates);
    assert_eq!(
        asked.lock().unwrap().last().map(String::as_str),
        Some("/owner/repo/main")
    );

    crate::library::skills::install_skills_at(
        &sb.store,
        &sb.homes,
        input,
        &["skills/pdf".to_owned()],
        &["claude".to_owned()],
    )
    .await
    .unwrap();
    assert_eq!(
        read_file(sb.home.join(".claude/skills/pdf/forms.md")),
        "forms"
    );

    // update: the library fetches again, the links go on pointing at it
    write(
        sb.home.join("magpie/library/skills/pdf/SKILL.md"),
        "---\nname: pdf\ndescription: old\n---\n",
    );
    crate::library::skills::update_skill_at(&sb.store, &sb.homes, "pdf")
        .await
        .unwrap();
    let v = crate::library::read_at(&sb.store, &sb.homes, &[]).unwrap();
    assert_eq!(v.skills[0].description, "PDFs one");
    assert_eq!(
        v.skills[0].source,
        format!("{input}/pdf"),
        "after update: {:?}",
        v.skills[0]
    );
    assert!(
        ours(&sb.home.join(".claude/skills/pdf"), "pdf", &sb.store),
        "claude's link went in the update"
    );

    let twice = crate::library::skills::install_skills_at(
        &sb.store,
        &sb.homes,
        input,
        &["skills/pdf".to_owned()],
        &[],
    )
    .await;
    assert!(twice.is_err(), "installed twice");
    crate::library::skills::set_tarball_base(None);
}
