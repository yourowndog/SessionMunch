-- V68 was applied on the canonical hub before these guards existed. An
-- applied migration is never edited (refinery rejects a changed checksum), so
-- the stricter guards are installed here instead.

-- `_global` is reserved: it can be neither an alias nor a rename source/target.
DROP TRIGGER IF EXISTS project_alias_collision;
CREATE TRIGGER project_alias_collision BEFORE INSERT ON project_aliases
WHEN NEW.name = '_global' OR EXISTS (
    SELECT 1 FROM projects owner JOIN projects other
      ON other.workspace_id = owner.workspace_id
    WHERE owner.id = NEW.project_id AND other.name = NEW.name
) OR EXISTS (
    SELECT 1 FROM projects owner JOIN projects other
      ON other.workspace_id = owner.workspace_id
    JOIN project_aliases a ON a.project_id = other.id
    WHERE owner.id = NEW.project_id AND a.name = NEW.name
      AND other.id != NEW.project_id
)
BEGIN
    SELECT RAISE(ABORT, 'project name or alias already exists');
END;

DROP TRIGGER IF EXISTS project_rename_alias_collision;
CREATE TRIGGER project_rename_alias_collision BEFORE UPDATE OF name, workspace_id ON projects
WHEN (OLD.name != NEW.name AND (OLD.name = '_global' OR NEW.name = '_global')) OR EXISTS (
    SELECT 1 FROM project_aliases a JOIN projects p ON p.id = a.project_id
    WHERE p.workspace_id = NEW.workspace_id AND a.name = NEW.name AND p.id != NEW.id
) OR EXISTS (
    SELECT 1 FROM project_aliases own JOIN projects other
      ON other.workspace_id = NEW.workspace_id AND other.id != NEW.id
    WHERE own.project_id = OLD.id AND (
      other.name = own.name OR EXISTS (
        SELECT 1 FROM project_aliases a WHERE a.project_id = other.id AND a.name = own.name
      )
    )
)
BEGIN
    SELECT RAISE(ABORT, 'project name or alias already exists');
END;

-- Aliases are immutable names; rename changes them via insert/delete only.
CREATE TRIGGER project_alias_no_update BEFORE UPDATE ON project_aliases
BEGIN
    SELECT RAISE(ABORT, 'project aliases cannot be updated');
END;
