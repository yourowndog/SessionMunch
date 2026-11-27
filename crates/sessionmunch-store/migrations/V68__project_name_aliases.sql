-- Names retained by rename are references to the same stable project, never copies.
CREATE TABLE project_aliases (
    project_id BLOB NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    PRIMARY KEY(project_id, name)
);
CREATE INDEX idx_project_aliases_name ON project_aliases(name);

-- One shared name-resolution relation for reads, creates, links, and filters.
CREATE VIEW project_names AS
    SELECT id AS project_id, workspace_id, name FROM projects
    UNION ALL
    SELECT p.id, p.workspace_id, a.name
      FROM project_aliases a JOIN projects p ON p.id = a.project_id;


-- Guard every SQL insertion, including rebuild/import and future writer paths.
CREATE TRIGGER project_alias_collision BEFORE INSERT ON project_aliases
WHEN EXISTS (
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

CREATE TRIGGER project_name_alias_collision BEFORE INSERT ON projects
WHEN EXISTS (
    SELECT 1 FROM project_aliases a JOIN projects p ON p.id = a.project_id
    WHERE p.workspace_id = NEW.workspace_id AND a.name = NEW.name
)
BEGIN
    SELECT RAISE(ABORT, 'project name or alias already exists');
END;

CREATE TRIGGER project_rename_alias_collision BEFORE UPDATE OF name, workspace_id ON projects
WHEN EXISTS (
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

-- Returning to a former name consumes that alias and retains the displaced name.
CREATE TRIGGER project_rename_retain_alias AFTER UPDATE OF name ON projects
WHEN OLD.name != NEW.name
BEGIN
    DELETE FROM project_aliases WHERE project_id = NEW.id AND name = NEW.name;
    INSERT OR IGNORE INTO project_aliases(project_id, name) VALUES (NEW.id, OLD.name);
END;
