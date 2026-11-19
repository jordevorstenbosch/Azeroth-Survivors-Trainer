use serde_json::{Value, json};
use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

pub struct Settings {
    pub directory: PathBuf,
    fields: Value,
}

impl Settings {
    pub fn load() -> io::Result<Self> {
        let directory =
            PathBuf::from(env::var_os("LOCALAPPDATA").ok_or_else(|| {
                io::Error::other("Windows user settings directory is unavailable.")
            })?)
            .join("Azeroth Survivors Trainer");
        fs::create_dir_all(&directory)?;
        let legacy = env::current_exe()?.with_file_name("session.json");
        let path = directory.join("settings.json");
        let contents = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => match fs::read(legacy) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Ok(Self {
                        directory,
                        fields: json!({}),
                    });
                }
                Err(error) => return Err(error),
            },
            Err(error) => return Err(error),
        };
        let source: Value = serde_json::from_slice(&contents).map_err(io::Error::other)?;
        if !source.is_object() {
            return Err(io::Error::other(
                "Trainer settings must contain a JSON object.",
            ));
        }
        // Carry over preferences only. Old process IDs and addresses are per run.
        let mut fields = json!({});
        for key in [
            "game_exe",
            "max_talent_class",
            "talent_cap",
            "auto_update",
            "gui_disabled",
        ] {
            if let Some(value) = source.get(key) {
                fields[key] = value.clone();
            }
        }
        if let Some(targets) = source
            .pointer("/config/targets")
            .or_else(|| source.get("targets"))
        {
            fields["targets"] = targets.clone();
        }
        Ok(Self { directory, fields })
    }

    pub fn game_path(&self) -> Option<&Path> {
        self.fields
            .get("game_exe")?
            .as_str()
            .filter(|path| !path.is_empty())
            .map(Path::new)
    }

    pub fn set_game_path(&mut self, path: &Path) -> io::Result<()> {
        let path = path
            .to_str()
            .ok_or_else(|| io::Error::other("Invalid game path."))?;
        let mut updated = self.fields.clone();
        updated["game_exe"] = json!(path);
        let destination = self.directory.join("settings.json");
        let temporary = self
            .directory
            .join(format!("settings-{}.tmp", std::process::id()));
        fs::write(
            &temporary,
            serde_json::to_vec_pretty(&updated).map_err(io::Error::other)?,
        )?;
        if let Err(error) = fs::rename(&temporary, destination) {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        self.fields = updated;
        Ok(())
    }

    pub fn talent_class(&self) -> &str {
        self.fields
            .get("max_talent_class")
            .and_then(Value::as_str)
            .unwrap_or("All classes")
    }

    pub fn talent_cap(&self) -> u64 {
        self.fields
            .get("talent_cap")
            .and_then(Value::as_u64)
            .unwrap_or(150)
    }
}
