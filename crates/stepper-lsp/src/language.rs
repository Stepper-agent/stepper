//! File-extension → LSP `languageId` mapping (a pragmatic subset of opencode's
//! `lsp/language.ts`). Used to label `textDocument/didOpen`.

use std::path::Path;

pub fn language_id(path: &Path) -> &'static str {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    // Compound extensions first (e.g. `.html.erb`).
    for (suffix, id) in COMPOUND {
        if name.ends_with(suffix) {
            return id;
        }
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    SIMPLE
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, id)| *id)
        .unwrap_or("plaintext")
}

const COMPOUND: &[(&str, &str)] = &[
    (".html.erb", "erb"),
    (".js.erb", "erb"),
    (".css.erb", "erb"),
    (".json.erb", "erb"),
];

const SIMPLE: &[(&str, &str)] = &[
    ("c", "c"),
    ("cc", "cpp"),
    ("cpp", "cpp"),
    ("cxx", "cpp"),
    ("c++", "cpp"),
    ("h", "cpp"),
    ("hpp", "cpp"),
    ("cs", "csharp"),
    ("css", "css"),
    ("scss", "scss"),
    ("sass", "sass"),
    ("less", "less"),
    ("clj", "clojure"),
    ("cljs", "clojure"),
    ("cljc", "clojure"),
    ("edn", "clojure"),
    ("dart", "dart"),
    ("d", "d"),
    ("ex", "elixir"),
    ("exs", "elixir"),
    ("erl", "erlang"),
    ("hrl", "erlang"),
    ("fs", "fsharp"),
    ("fsi", "fsharp"),
    ("fsx", "fsharp"),
    ("go", "go"),
    ("gleam", "gleam"),
    ("groovy", "groovy"),
    ("hs", "haskell"),
    ("html", "html"),
    ("htm", "html"),
    ("java", "java"),
    ("jl", "julia"),
    ("js", "javascript"),
    ("mjs", "javascript"),
    ("cjs", "javascript"),
    ("jsx", "javascriptreact"),
    ("json", "json"),
    ("jsonc", "jsonc"),
    ("kt", "kotlin"),
    ("kts", "kotlin"),
    ("lua", "lua"),
    ("md", "markdown"),
    ("markdown", "markdown"),
    ("ml", "ocaml"),
    ("mli", "ocaml"),
    ("m", "objective-c"),
    ("mm", "objective-cpp"),
    ("nix", "nix"),
    ("php", "php"),
    ("pl", "perl"),
    ("pm", "perl"),
    ("ps1", "powershell"),
    ("py", "python"),
    ("pyi", "python"),
    ("r", "r"),
    ("rb", "ruby"),
    ("rake", "ruby"),
    ("gemspec", "ruby"),
    ("ru", "ruby"),
    ("erb", "erb"),
    ("rs", "rust"),
    ("scala", "scala"),
    ("sh", "shellscript"),
    ("bash", "shellscript"),
    ("zsh", "shellscript"),
    ("sql", "sql"),
    ("svelte", "svelte"),
    ("swift", "swift"),
    ("ts", "typescript"),
    ("mts", "typescript"),
    ("cts", "typescript"),
    ("tsx", "typescriptreact"),
    ("tf", "terraform"),
    ("tfvars", "terraform-vars"),
    ("toml", "toml"),
    ("vue", "vue"),
    ("xml", "xml"),
    ("yaml", "yaml"),
    ("yml", "yaml"),
    ("zig", "zig"),
    ("zon", "zig"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn maps_common_extensions() {
        assert_eq!(language_id(Path::new("a/b/main.rs")), "rust");
        assert_eq!(language_id(Path::new("x.go")), "go");
        assert_eq!(language_id(Path::new("y.tsx")), "typescriptreact");
        assert_eq!(language_id(Path::new("z.PY")), "python"); // case-insensitive
    }

    #[test]
    fn compound_extension_wins() {
        assert_eq!(language_id(Path::new("view.html.erb")), "erb");
    }

    #[test]
    fn unknown_is_plaintext() {
        assert_eq!(language_id(Path::new("file.unknownext")), "plaintext");
        assert_eq!(language_id(Path::new("noext")), "plaintext");
    }
}
