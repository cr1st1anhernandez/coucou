// The prompt library: <Documents>\mochi, one folder per project with prompts,
// scripts and notes. Plain files, so the user (or Claude Code) can edit them by
// hand; the island only lists them and copies them where they're needed.
//
// The folder comes from Windows' own Documents location for whoever is signed
// in — never a hard-coded user name — so it follows OneDrive redirection and
// works the same on every PC.

use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::{AppHandle, Manager};

const FOLDER: &str = "mochi";
const KINDS: [(&str, Kind); 3] = [("prompts", Kind::Prompt), ("scripts", Kind::Script), ("notas", Kind::Note)];
/// Files bigger than this are listed but not previewed.
const MAX_PREVIEW_BYTES: u64 = 256 * 1024;
const PREVIEW_CHARS: usize = 160;

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub enum Kind {
    Prompt,
    Script,
    Note,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub kind: Kind,
    pub title: String,
    /// File name, e.g. `revisar-prs.md`.
    pub file: String,
    pub path: String,
    pub preview: String,
    pub tags: Vec<String>,
    pub order: f64,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub id: String,
    pub name: String,
    pub color: Option<String>,
    /// The repo folder, from proyecto.json — where "A Warp" opens.
    pub repo: Option<String>,
    pub items: Vec<Item>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Library {
    pub dir: String,
    pub projects: Vec<Project>,
}

/// <Documents>\mochi, created on first use.
pub fn dir(app: &AppHandle) -> Result<PathBuf, String> {
    let docs = app
        .path()
        .document_dir()
        .map_err(|e| format!("No encontré la carpeta Documentos: {e}"))?;
    let dir = docs.join(FOLDER);
    std::fs::create_dir_all(&dir).map_err(|e| format!("No se pudo crear {}: {e}", dir.display()))?;
    Ok(dir)
}

pub fn list(app: &AppHandle) -> Result<Library, String> {
    let root = dir(app)?;
    Ok(Library { dir: root.to_string_lossy().to_string(), projects: scan(&root) })
}

fn scan(root: &Path) -> Vec<Project> {
    let Ok(entries) = std::fs::read_dir(root) else { return Vec::new() };
    let mut projects: Vec<Project> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| project(&e.path()))
        .collect();
    projects.sort_by_key(|p| p.name.to_lowercase());
    projects
}

fn project(path: &Path) -> Project {
    let id = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let meta: serde_json::Value = std::fs::read(path.join("proyecto.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let text = |k: &str| meta.get(k).and_then(|v| v.as_str()).map(str::to_string).filter(|s| !s.is_empty());

    let mut items = Vec::new();
    for (folder, kind) in KINDS {
        let Ok(files) = std::fs::read_dir(path.join(folder)) else { continue };
        for f in files.flatten() {
            let p = f.path();
            if p.is_file() {
                items.push(item(&p, kind));
            }
        }
    }
    items.sort_by(|a, b| {
        a.order.partial_cmp(&b.order).unwrap_or(std::cmp::Ordering::Equal).then(a.title.cmp(&b.title))
    });
    Project { name: text("nombre").unwrap_or_else(|| id.clone()), id, color: text("color"), repo: text("ruta"), items }
}

fn item(path: &Path, kind: Kind) -> Item {
    let file = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let small = std::fs::metadata(path).map(|m| m.len() <= MAX_PREVIEW_BYTES).unwrap_or(false);
    let raw = if small { std::fs::read_to_string(path).unwrap_or_default() } else { String::new() };
    let (front, body) = split_front_matter(&raw);
    let field = |k: &str| {
        front.iter().find(|(key, _)| key.eq_ignore_ascii_case(k)).map(|(_, v)| v.clone())
    };
    let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let title = field("titulo").or_else(|| field("title")).unwrap_or_else(|| stem.replace(['-', '_'], " "));
    let tags = field("etiquetas")
        .or_else(|| field("tags"))
        .map(|t| t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let order = field("orden").and_then(|o| o.parse().ok()).unwrap_or(1000.0);
    let preview: String = body.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(PREVIEW_CHARS).collect();
    Item { kind, title, file, path: path.to_string_lossy().to_string(), preview, tags, order }
}

/// `---\ntitulo: X\n---\nbody` → ([("titulo", "X")], "body"). No front matter → ([], whole text).
fn split_front_matter(text: &str) -> (Vec<(String, String)>, &str) {
    let trimmed = text.trim_start_matches('\u{feff}');
    let Some(rest) = trimmed.strip_prefix("---") else { return (Vec::new(), trimmed) };
    let rest = rest.trim_start_matches(['\r', '\n']);
    let Some(end) = rest.find("\n---") else { return (Vec::new(), trimmed) };
    let fields = rest[..end]
        .lines()
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().trim_matches('"').to_string()))
        .collect();
    let after = &rest[end + 4..];
    (fields, after.trim_start_matches(['\r', '\n']))
}

/// What "Copiar" puts on the clipboard: a prompt's text without its front
/// matter; a script or note exactly as it is on disk.
pub fn content_for_copy(app: &AppHandle, path: &str, kind: Kind) -> Result<String, String> {
    let path = inside_library(app, path)?;
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("No se pudo leer: {e}"))?;
    Ok(match kind {
        Kind::Prompt => split_front_matter(&raw).1.trim().to_string(),
        _ => raw,
    })
}

/// Refuses any path outside the library: the island can't read arbitrary files.
pub fn inside_library(app: &AppHandle, path: &str) -> Result<PathBuf, String> {
    let root = dir(app)?.canonicalize().map_err(|e| e.to_string())?;
    let p = Path::new(path).canonicalize().map_err(|_| "Ese archivo ya no existe.".to_string())?;
    if p.starts_with(&root) {
        Ok(p)
    } else {
        Err("Ese archivo no está en la biblioteca.".into())
    }
}

/// The path shown and copied: without the `\\?\` prefix canonicalize adds.
pub fn display_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn front_matter_is_split_from_the_body() {
        let (f, body) = split_front_matter("---\ntitulo: Revisa PRs\netiquetas: git, review\norden: 2\n---\nRevisa mis PRs.\n");
        assert_eq!(f[0], ("titulo".into(), "Revisa PRs".into()));
        assert_eq!(f.len(), 3);
        assert_eq!(body.trim(), "Revisa mis PRs.");
        let (f, body) = split_front_matter("solo texto");
        assert!(f.is_empty());
        assert_eq!(body, "solo texto");
        let (_, body) = split_front_matter("---\r\ntitulo: X\r\n---\r\nhola");
        assert_eq!(body, "hola");
    }

    #[test]
    fn a_project_lists_its_files_by_kind_and_order() {
        let root = std::env::temp_dir().join(format!("coucou-lib-{}", std::process::id()));
        let p = root.join("api-pagos");
        std::fs::create_dir_all(p.join("prompts")).unwrap();
        std::fs::create_dir_all(p.join("scripts")).unwrap();
        std::fs::create_dir_all(p.join("notas")).unwrap();
        std::fs::write(p.join("proyecto.json"), r##"{"ruta":"C:\\repo","color":"#22C55E"}"##).unwrap();
        std::fs::write(p.join("prompts").join("b.md"), "---\ntitulo: Segundo\norden: 2\n---\nB").unwrap();
        std::fs::write(p.join("prompts").join("a.md"), "---\ntitulo: Primero\norden: 1\n---\nA").unwrap();
        std::fs::write(p.join("scripts").join("levantar.ps1"), "docker compose up -d").unwrap();
        std::fs::write(p.join("notas").join("accesos-prueba.md"), "qa / qa123").unwrap();

        let projects = scan(&root);
        assert_eq!(projects.len(), 1);
        let proj = &projects[0];
        assert_eq!(proj.repo.as_deref(), Some(r"C:\repo"));
        let prompts: Vec<_> = proj.items.iter().filter(|i| i.kind == Kind::Prompt).map(|i| i.title.as_str()).collect();
        assert_eq!(prompts, ["Primero", "Segundo"]);
        assert!(proj.items.iter().any(|i| i.kind == Kind::Script && i.preview == "docker compose up -d"));
        assert!(proj.items.iter().any(|i| i.kind == Kind::Note && i.title == "accesos prueba"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
