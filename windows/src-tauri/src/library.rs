// The library: <Documents>\mochi, one folder per project with three kinds of
// things the user keeps for Claude Code: instructions (how they want Claude to
// work, and how to bring an environment up), accesses (dev database
// credentials as `clave: valor` lines) and documents (.docx user stories and
// templates). Plain files, so the user (or Claude Code) can edit them by hand;
// the island lists them, copies them where they're needed and takes in files
// dropped on it.
//
// The folder comes from Windows' own Documents location for whoever is signed
// in — never a hard-coded user name — so it follows OneDrive redirection and
// works the same on every PC.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

const FOLDER: &str = "mochi";
const KINDS: [(&str, Kind); 3] = [
    ("instrucciones", Kind::Instruction),
    ("accesos", Kind::Access),
    ("documentos", Kind::Document),
];
/// The first version's folders, moved into the new ones the first time a
/// project is listed.
const LEGACY: [&str; 3] = ["prompts", "scripts", "notas"];
/// Files bigger than this are listed but not previewed.
const MAX_PREVIEW_BYTES: u64 = 256 * 1024;
const PREVIEW_CHARS: usize = 160;
/// An access shows at most this many `clave: valor` lines.
const MAX_FIELDS: usize = 12;
/// Read as text; anything else is a document.
const TEXT_EXTENSIONS: [&str; 12] = ["md", "txt", "markdown", "ps1", "sh", "bat", "cmd", "py", "js", "ts", "json", "env"];

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub enum Kind {
    Instruction,
    Access,
    Document,
}

impl Kind {
    pub fn folder(self) -> &'static str {
        KINDS.iter().find(|(_, k)| *k == self).map(|(f, _)| *f).unwrap_or("instrucciones")
    }
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub kind: Kind,
    /// Instructions: "prompt" or "entorno". Documents: "hu", "plantilla" or
    /// "otro". Accesses: "".
    pub sub: String,
    pub title: String,
    /// File name, e.g. `revisar-prs.md`.
    pub file: String,
    pub path: String,
    pub preview: String,
    pub tags: Vec<String>,
    pub order: f64,
    /// An access's `clave: valor` lines, in file order.
    pub fields: Vec<(String, String)>,
    /// Accesses: the environment it's for (`entorno:` in the front matter, "dev" by default).
    pub env: String,
    pub size: u64,
    /// Last change, in milliseconds since 1970.
    pub modified: Option<f64>,
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

/// What a dropped file became.
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Imported {
    pub kind: Kind,
    pub title: String,
    pub path: String,
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
        .map(|e| {
            migrate(&e.path());
            project(&e.path())
        })
        .collect();
    projects.sort_by_key(|p| p.name.to_lowercase());
    projects
}

/// prompts\ and scripts\ become instrucciones\; notas\ go to accesos\ when they
/// hold credentials and to instrucciones\ otherwise. Never overwrites: a file
/// whose name is already taken stays where it was.
fn migrate(project: &Path) {
    for legacy in LEGACY {
        let from = project.join(legacy);
        let Ok(files) = std::fs::read_dir(&from) else { continue };
        for f in files.flatten() {
            let p = f.path();
            if !p.is_file() {
                continue;
            }
            let kind = match legacy {
                "notas" if looks_like_access(&read_small(&p)) => Kind::Access,
                _ => Kind::Instruction,
            };
            let dest_dir = project.join(kind.folder());
            let dest = dest_dir.join(f.file_name());
            if dest.exists() || std::fs::create_dir_all(&dest_dir).is_err() {
                continue;
            }
            if let Err(e) = std::fs::rename(&p, &dest) {
                crate::log::line(format!("library: could not move {}: {e}", p.display()));
            }
        }
        // Only goes when empty.
        let _ = std::fs::remove_dir(&from);
    }
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
            let hidden = f.file_name().to_string_lossy().starts_with(['.', '~']);
            if p.is_file() && !hidden {
                items.push(item(&p, kind));
            }
        }
    }
    items.sort_by(|a, b| {
        a.order.partial_cmp(&b.order).unwrap_or(std::cmp::Ordering::Equal).then(a.title.cmp(&b.title))
    });
    Project { name: text("nombre").unwrap_or_else(|| id.clone()), id, color: text("color"), repo: text("ruta"), items }
}

fn is_text(path: &Path) -> bool {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    ext.is_empty() || TEXT_EXTENSIONS.contains(&ext.as_str())
}

fn read_small(path: &Path) -> String {
    let small = std::fs::metadata(path).map(|m| m.len() <= MAX_PREVIEW_BYTES).unwrap_or(false);
    if small && is_text(path) { std::fs::read_to_string(path).unwrap_or_default() } else { String::new() }
}

fn item(path: &Path, kind: Kind) -> Item {
    let file = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let meta = std::fs::metadata(path).ok();
    let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let modified = meta
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as f64);
    let raw = read_small(path);
    let (front, body) = split_front_matter(&raw);
    let field = |k: &str| {
        front.iter().find(|(key, _)| key.eq_ignore_ascii_case(k)).map(|(_, v)| v.clone())
    };
    let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let title = match kind {
        // A document's name is the one the user gave the file.
        Kind::Document => stem.clone(),
        _ => field("titulo").or_else(|| field("title")).unwrap_or_else(|| stem.replace(['-', '_'], " ")),
    };
    let tags = field("etiquetas")
        .or_else(|| field("tags"))
        .map(|t| t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let order = field("orden").and_then(|o| o.parse().ok()).unwrap_or(1000.0);
    let preview: String = body.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(PREVIEW_CHARS).collect();
    let sub = match kind {
        Kind::Instruction => field("tipo")
            .map(|t| t.to_lowercase())
            .filter(|t| t == "prompt" || t == "entorno")
            .unwrap_or_else(|| instruction_sub(path).to_string()),
        Kind::Document => document_sub(&file).to_string(),
        Kind::Access => String::new(),
    };
    let fields = if kind == Kind::Access { fields_of(body) } else { Vec::new() };
    let env = if kind == Kind::Access { field("entorno").unwrap_or_else(|| "dev".into()) } else { String::new() };
    Item {
        kind,
        sub,
        title,
        file,
        path: path.to_string_lossy().to_string(),
        preview,
        tags,
        order,
        fields,
        env,
        size,
        modified,
    }
}

/// "entorno" for how-to-bring-it-up notes and scripts, "prompt" for the rest.
fn instruction_sub(path: &Path) -> &'static str {
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    let script = !name.ends_with(".md") && !name.ends_with(".txt") && path.extension().is_some();
    let words = ["levantar", "entorno", "setup", "instalar", "arrancar", "docker", "ambiente", "correr"];
    if script || words.iter().any(|w| name.contains(w)) { "entorno" } else { "prompt" }
}

fn document_sub(file: &str) -> &'static str {
    let name = file.to_lowercase();
    if name.contains("plantilla") || name.contains("template") {
        "plantilla"
    } else if name.starts_with("hu") || name.contains("historia") || name.contains("user story") {
        "hu"
    } else {
        "otro"
    }
}

/// The `clave: valor` lines of an access, markdown decoration stripped:
/// `- **host:** \`localhost\`` → ("host", "localhost").
fn fields_of(body: &str) -> Vec<(String, String)> {
    let clean = |s: &str| s.trim_matches(|c: char| c.is_whitespace() || "*`\"'".contains(c)).to_string();
    body.lines()
        .map(|l| l.trim().trim_start_matches(['-', '*', '+']).trim())
        .filter(|l| !l.starts_with('#') && !l.starts_with('|') && !l.starts_with('>'))
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (clean(k), clean(v)))
        .filter(|(k, v)| !k.is_empty() && !v.is_empty() && k.chars().count() <= 24 && !v.starts_with("//"))
        .take(MAX_FIELDS)
        .collect()
}

/// True when a text has the look of credentials: a host, a user or a password.
fn looks_like_access(text: &str) -> bool {
    let t = text.to_lowercase();
    ["password", "contraseña", "contrasena", "usuario:", "user:", "host:", "token"].iter().any(|w| t.contains(w))
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

/// What "Copiar" puts on the clipboard: an instruction's text without its
/// front matter; an access exactly as it is on disk.
pub fn content_for_copy(app: &AppHandle, path: &str, kind: Kind) -> Result<String, String> {
    let path = inside_library(app, path)?;
    if !is_text(&path) {
        return Err("Ese archivo no es de texto: usa Copiar archivo.".into());
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("No se pudo leer: {e}"))?;
    Ok(match kind {
        Kind::Instruction => split_front_matter(&raw).1.trim().to_string(),
        _ => raw,
    })
}

/// A file dropped on the island goes into `project`: in `kind` when it was
/// dropped on a category, or wherever it fits otherwise. A document always
/// lands in documentos. The original stays where it was.
pub fn import(app: &AppHandle, source: &str, project: &str, kind: Option<Kind>) -> Result<Imported, String> {
    let root = dir(app)?;
    let project_dir = root.join(project);
    if project.is_empty() || project.contains(['\\', '/']) || project.starts_with('.') || !project_dir.is_dir() {
        return Err("Ese proyecto ya no existe.".into());
    }
    let source = Path::new(source);
    if source.is_dir() {
        return Err("Suelta archivos, no carpetas.".into());
    }
    if !source.is_file() {
        return Err("No encontré ese archivo.".into());
    }
    let auto = || {
        if !is_text(source) {
            Kind::Document
        } else if looks_like_access(&read_small(source)) {
            Kind::Access
        } else {
            Kind::Instruction
        }
    };
    let kind = match kind {
        _ if !is_text(source) => Kind::Document,
        Some(k) => k,
        None => auto(),
    };
    let dest_dir = project_dir.join(kind.folder());
    std::fs::create_dir_all(&dest_dir).map_err(|e| format!("No se pudo crear {}: {e}", dest_dir.display()))?;
    let dest = free_name(&dest_dir, source);
    std::fs::copy(source, &dest).map_err(|e| format!("No se pudo copiar: {e}"))?;
    let saved = item(&dest, kind);
    Ok(Imported { kind, title: saved.title, path: saved.path })
}

/// `name.ext`, or `name (2).ext`, `name (3).ext`… — whichever isn't taken yet.
fn free_name(dir: &Path, source: &Path) -> PathBuf {
    let name = source.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "archivo".into());
    let first = dir.join(&name);
    if !first.exists() {
        return first;
    }
    let stem = source.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let ext = source.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (2..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .unwrap_or(first)
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

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("coucou-lib-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

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
    fn access_fields_come_from_clave_valor_lines() {
        let f = fields_of("# Postgres\n\n- **host:** `localhost`\npuerto: 5432\npassword: a:b\nVer https://x.dev\n| a | b |\n");
        assert_eq!(
            f,
            [
                ("host".to_string(), "localhost".to_string()),
                ("puerto".into(), "5432".into()),
                ("password".into(), "a:b".into()),
            ]
        );
    }

    #[test]
    fn a_project_lists_its_files_by_kind_and_order() {
        let root = temp_root("list");
        let p = root.join("api-pagos");
        for k in ["instrucciones", "accesos", "documentos"] {
            std::fs::create_dir_all(p.join(k)).unwrap();
        }
        std::fs::write(p.join("proyecto.json"), r##"{"ruta":"C:\\repo","color":"#22C55E"}"##).unwrap();
        std::fs::write(p.join("instrucciones").join("b.md"), "---\ntitulo: Segundo\norden: 2\n---\nB").unwrap();
        std::fs::write(p.join("instrucciones").join("a.md"), "---\ntitulo: Primero\norden: 1\n---\nA").unwrap();
        std::fs::write(p.join("instrucciones").join("levantar-api.md"), "docker compose up -d").unwrap();
        std::fs::write(p.join("accesos").join("postgres.md"), "---\nentorno: qa\n---\nhost: localhost\npassword: dev").unwrap();
        std::fs::write(p.join("documentos").join("HU-01 Login.docx"), [0u8, 1, 2]).unwrap();
        std::fs::write(p.join("documentos").join("Plantilla HU.docx"), [0u8]).unwrap();

        let projects = scan(&root);
        assert_eq!(projects.len(), 1);
        let proj = &projects[0];
        assert_eq!(proj.repo.as_deref(), Some(r"C:\repo"));
        let prompts: Vec<_> =
            proj.items.iter().filter(|i| i.kind == Kind::Instruction && i.sub == "prompt").map(|i| i.title.as_str()).collect();
        assert_eq!(prompts, ["Primero", "Segundo"]);
        assert!(proj.items.iter().any(|i| i.sub == "entorno" && i.title == "levantar api"));
        let access = proj.items.iter().find(|i| i.kind == Kind::Access).unwrap();
        assert_eq!(access.env, "qa");
        assert_eq!(access.fields.len(), 2);
        let hu = proj.items.iter().find(|i| i.title == "HU-01 Login").unwrap();
        assert_eq!((hu.sub.as_str(), hu.size), ("hu", 3));
        assert!(proj.items.iter().any(|i| i.sub == "plantilla"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_old_folders_move_into_the_new_ones() {
        let root = temp_root("migrate");
        let p = root.join("duo");
        for k in LEGACY {
            std::fs::create_dir_all(p.join(k)).unwrap();
        }
        std::fs::write(p.join("prompts").join("flujo.md"), "Trabaja en un worktree").unwrap();
        std::fs::write(p.join("scripts").join("levantar.ps1"), "docker compose up").unwrap();
        std::fs::write(p.join("notas").join("credenciales.md"), "usuario: qa\ncontraseña: qa123").unwrap();
        std::fs::write(p.join("notas").join("ideas.md"), "Algún día").unwrap();

        let proj = &scan(&root)[0];
        let kinds: Vec<_> = proj.items.iter().map(|i| (i.file.as_str(), i.kind)).collect();
        assert!(kinds.contains(&("flujo.md", Kind::Instruction)));
        assert!(kinds.contains(&("levantar.ps1", Kind::Instruction)));
        assert!(kinds.contains(&("credenciales.md", Kind::Access)));
        assert!(kinds.contains(&("ideas.md", Kind::Instruction)));
        assert!(LEGACY.iter().all(|k| !p.join(k).exists()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_taken_name_gets_a_number() {
        let root = temp_root("names");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("HU.docx"), "x").unwrap();
        std::fs::write(root.join("HU (2).docx"), "x").unwrap();
        assert_eq!(free_name(&root, Path::new(r"C:\Descargas\HU.docx")), root.join("HU (3).docx"));
        assert_eq!(free_name(&root, Path::new(r"C:\Descargas\otra.md")), root.join("otra.md"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
