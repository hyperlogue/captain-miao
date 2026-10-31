//! Conservative defaults for commands whose purpose is to stay running.
//! This list only classifies commands already reported as background tasks;
//! it never discovers processes or changes a launcher's state. Unknown forms,
//! compound shell programs and finite builds/tests remain Task.

const IDLE_PREFIXES: &[&[&str]] = &[
    &["cargo", "watch"],
    &["next", "dev"],
    &["next", "start"],
    &["nuxt", "dev"],
    &["astro", "dev"],
    &["gatsby", "develop"],
    &["remix", "dev"],
    &["flask", "run"],
    &["rails", "server"],
    &["rails", "s"],
    &["expo", "start"],
    &["react-native", "start"],
    &["python", "-m", "http.server"],
    &["python3", "-m", "http.server"],
    &["ng", "serve"],
    &["webpack", "serve"],
    &["php", "artisan", "serve"],
    &["hugo", "server"],
    &["mkdocs", "serve"],
    &["streamlit", "run"],
    &["trunk", "serve"],
    &["wrangler", "dev"],
    &["netlify", "dev"],
    &["vercel", "dev"],
    &["storybook", "dev"],
    &["tsx", "watch"],
    &["r3", "watch"],
];

const IDLE_PROGRAMS: &[&str] = &[
    "nodemon",
    "watchexec",
    "entr",
    "webpack-dev-server",
    "http-server",
    "live-server",
    "json-server",
    "browser-sync",
    "livereload",
    "uvicorn",
    "gunicorn",
    "hypercorn",
    "daphne",
    "puma",
    "rackup",
    "start-storybook",
    "bacon",
    "air",
    "ts-node-dev",
    "metro",
    "cargo-watch",
];

const IDLE_SCRIPTS: &[&str] = &["dev", "serve", "start", "watch", "develop", "server"];

pub(super) fn is_idle(command: &str) -> bool {
    classify(command, 0)
}

fn classify(command: &str, depth: usize) -> bool {
    // Do not guess which stage of a pipeline/compound program is still alive.
    // Users can explicitly classify these through the same preference rules.
    if depth > 8 || command.contains([';', '|', '&', '\n', '`', '$', '<', '>']) {
        return false;
    }
    let Ok(words) = shell_words::split(command) else {
        return false;
    };
    classify_words(&words, depth)
}

fn classify_words(words: &[String], depth: usize) -> bool {
    let Some(first) = words.first() else {
        return false;
    };
    if depth > 8 {
        return false;
    }
    let program = first.rsplit('/').next().unwrap_or(first);
    let args = &words[1..];

    // Shell assignments and common runners preserve the command's identity.
    if first.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name
                .chars()
                .enumerate()
                .all(|(i, c)| c == '_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()))
    }) || matches!(program, "env" | "exec")
    {
        return classify_words(args, depth + 1);
    }
    if matches!(program, "bash" | "sh" | "zsh")
        && args
            .first()
            .is_some_and(|s| matches!(s.as_str(), "-c" | "-lc"))
    {
        return args
            .get(1)
            .is_some_and(|script| classify(script, depth + 1));
    }
    if program == "nix" && args.first().is_some_and(|s| s == "develop" || s == "shell") {
        return args
            .iter()
            .position(|s| s == "--command" || s == "-c")
            .is_some_and(|i| classify_words(&args[i + 1..], depth + 1));
    }
    if matches!(program, "npx" | "bunx") {
        let args = if args.first().is_some_and(|s| s == "-y" || s == "--yes") {
            &args[1..]
        } else {
            args
        };
        return classify_words(args, depth + 1);
    }
    if program == "uv" && args.first().is_some_and(|s| s == "run") {
        return classify_words(&args[1..], depth + 1);
    }

    if IDLE_PREFIXES.iter().any(|prefix| {
        prefix[0] == program
            && args.len() >= prefix.len() - 1
            && args
                .iter()
                .zip(&prefix[1..])
                .all(|(arg, expected)| arg == expected)
    }) || IDLE_PROGRAMS.contains(&program)
    {
        return true;
    }
    if matches!(program, "npm" | "pnpm" | "yarn" | "bun") {
        let script = if args.first().is_some_and(|s| s == "run") {
            args.get(1)
        } else {
            args.first()
        };
        if script.is_some_and(|s| IDLE_SCRIPTS.contains(&s.as_str())) {
            return true;
        }
    }
    // Vite's default command serves; `vite build`/`preview` are not inferred
    // from the tool's name alone. Recognize only its explicit server forms.
    if program == "vite" {
        return !args.iter().any(|s| s == "build" || s == "preview")
            && args
                .first()
                .is_none_or(|s| s.starts_with('-') || matches!(s.as_str(), "dev" | "serve"));
    }
    if program == "node" && args.first().is_some_and(|s| s == "--watch") {
        return true;
    }
    if matches!(program, "tsc" | "jest" | "vitest" | "webpack")
        && args
            .iter()
            .any(|s| matches!(s.as_str(), "--watch" | "--watchAll"))
    {
        return true;
    }
    // The run-from-source r3 entrypoint is a review wait, just like `r3 watch`.
    matches!(program, "bun" | "node")
        && args
            .first()
            .is_some_and(|s| s.ends_with("/r3/cli/index.ts"))
        && args.get(1).is_some_and(|s| s == "watch")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servers_watchers_and_review_waits_are_idle() {
        for command in [
            "npm run dev -- --port 3000",
            "pnpm dev",
            "yarn start",
            "bun run watch",
            "npx -y vite",
            "/usr/bin/nodemon app.js",
            "cargo watch -x build",
            "python3 -m http.server 8000",
            "uv run uvicorn app:app --reload",
            "NODE_ENV=development npm run dev",
            "env PORT=3000 npm run dev",
            "nix develop --command bash -c 'r3 watch review_abc'",
            "r3 watch review_abc",
            "/opt/bin/r3 watch other-id",
            "bun /opt/r3/cli/index.ts watch review_abc",
            "jest --watchAll",
            "node --watch app.js",
            "tsx watch script.ts",
            "wrangler dev",
            "mkdocs serve",
        ] {
            assert!(is_idle(command), "{command}");
        }
    }

    #[test]
    fn finite_commands_and_ambiguous_shell_programs_stay_task() {
        for command in [
            "cargo build",
            "cargo test",
            "npm run build",
            "npm run test",
            "vite build",
            "vite --config vite.config.ts build",
            "npx vite build",
            "next build",
            "tsx script.ts",
            "tsx script.ts --watch",
            "node script.js --watch",
            "mkdocs build",
            "wrangler deploy",
            "python3 server.py",
            "my-server",
            "echo r3 watch review_abc",
            "printf '%s' 'npm run dev'",
            "rg --watch README.md",
            "r3 submit review_abc",
            "r3 list",
            "bun /opt/another/cli/index.ts watch review_abc",
            "npm run develop-once",
            "npm run dev && cargo test",
            "npm run dev; cargo test",
            "npm run dev|cargo test",
            "npm run dev\ncargo test",
            "npm run dev $(cargo test)",
            "bash -c 'npm run dev && cargo test'",
            "'unterminated",
            "",
            "cargo test -- --ignored",
        ] {
            assert!(!is_idle(command), "{command}");
        }
    }
}
