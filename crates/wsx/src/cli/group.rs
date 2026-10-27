use super::*;

pub(super) fn cmd_group_ls() -> Result<()> {
    let config = load_config()?;
    for group in &config.groups {
        println!("{}", group);
    }
    Ok(())
}

pub(super) fn validate_new_group(config: &GlobalConfig, name: &str) -> Result<()> {
    GroupKey::named(name.to_string()).map_err(anyhow::Error::msg)?;
    if config.named_group_exists(name) {
        bail!("group '{}' already exists", name);
    }
    Ok(())
}

pub(super) fn cmd_group_create(name: &str) -> Result<()> {
    let mut config = load_config()?;
    validate_new_group(&config, name)?;
    config.groups.push(name.to_string());
    config.save()?;
    println!("created group: {}", name);
    Ok(())
}

pub(super) fn cmd_group_rename(old: &str, new_name: &str) -> Result<()> {
    let mut config = load_config()?;
    let index = config
        .groups
        .iter()
        .position(|group| group == old)
        .ok_or_else(|| anyhow::anyhow!("group '{}' not found", old))?;
    validate_new_group(&config, new_name)?;
    config.groups[index] = new_name.to_string();
    for entry in &mut config.projects {
        for group in &mut entry.groups {
            if group == old {
                *group = new_name.to_string();
            }
        }
    }
    config.save()?;
    println!("renamed group: {} → {}", old, new_name);
    Ok(())
}

pub(super) fn project_path(config: &GlobalConfig, project: &str) -> Result<PathBuf> {
    config
        .projects
        .iter()
        .find(|entry| entry.name == project)
        .map(|entry| entry.path.clone())
        .ok_or_else(|| anyhow::anyhow!("project '{}' not found", project))
}

pub(super) fn cmd_group_add(group: &str, project: &str) -> Result<()> {
    let mut config = load_config()?;
    GroupKey::named(group.to_string()).map_err(anyhow::Error::msg)?;
    if !config.named_group_exists(group) {
        bail!("group '{}' not found", group);
    }
    let path = project_path(&config, project)?;
    if config
        .project_groups(&path)
        .iter()
        .any(|name| name == group)
    {
        println!("project '{}' is already in group '{}'", project, group);
        return Ok(());
    }
    config.add_project_to_group(&path, group);
    config.save()?;
    println!("added '{}' to group '{}'", project, group);
    Ok(())
}

pub(super) fn cmd_group_remove(group: &str, project: &str) -> Result<()> {
    let mut config = load_config()?;
    GroupKey::named(group.to_string()).map_err(anyhow::Error::msg)?;
    if !config.named_group_exists(group) {
        bail!("group '{}' not found", group);
    }
    let path = project_path(&config, project)?;
    if !config.remove_project_from_group(&path, group) {
        println!("project '{}' is not in group '{}'", project, group);
        return Ok(());
    }
    config.save()?;
    println!("removed '{}' from group '{}'", project, group);
    Ok(())
}
