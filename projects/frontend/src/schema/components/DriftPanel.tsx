import { useState } from 'react';
import type { ReactNode } from 'react';

function DriftSection({ title, titleClass, description, children }: { title: string; titleClass: string; description: string; children: ReactNode }) {
  return (
    <div className="drift-section">
      <h3 className={`drift-section-title ${titleClass}`}>{title}</h3>
      <p className="drift-section-desc">{description}</p>
      {children}
    </div>
  );
}

export function DriftPanel({ drift, onGoTo }: { drift: DriftReport; onGoTo: (id: string) => void }) {
  const [open, setOpen] = useState(false);

  if (drift.totalIssues === 0) {
    return (
      <button id="drift-btn" className="clean" title="No config drift">
        &#x2713;
      </button>
    );
  }

  const goTo = (id: string) => {
    onGoTo(id);
    setOpen(false);
  };

  const cd = drift.constraintDrift;

  return (
    <>
      <button id="drift-btn" className="has-issues" onClick={() => setOpen((v) => !v)} title="Config drift detected">
        &#x26A0; {drift.totalIssues}
      </button>

      <div id="drift-panel" className={open ? 'open' : ''}>
        <div id="drift-header">
          <h2>Config Drift</h2>
          <button id="drift-close" onClick={() => setOpen(false)}>
            &times;
          </button>
        </div>

        {drift.unassignedTables.length > 0 && (
          <DriftSection title={`Unassigned Tables (${drift.unassignedTables.length})`} titleClass="amber" description="In database but not in any domain">
            {drift.unassignedTables.map((name) => (
              <div key={name} className="drift-item clickable" onClick={() => goTo(name)}>
                {name}
              </div>
            ))}
          </DriftSection>
        )}

        {drift.ghostTables.length > 0 && (
          <DriftSection title={`Ghost Tables (${drift.ghostTables.length})`} titleClass="red" description="In config but not in database">
            {drift.ghostTables.map((name) => (
              <div key={name} className="drift-item">
                {name}
              </div>
            ))}
          </DriftSection>
        )}

        {drift.unmappedFkColumns.length > 0 && (
          <DriftSection title={`Unmapped FK Columns (${drift.unmappedFkColumns.length})`} titleClass="blue" description="Look like FKs but have no mapping">
            {drift.unmappedFkColumns.map(({ table, column }) => (
              <div key={`${table}.${column}`} className="drift-item clickable" onClick={() => goTo(table)}>
                <span className="drift-table">{table}</span>.<span className="drift-col">{column}</span>
              </div>
            ))}
          </DriftSection>
        )}

        {drift.invalidFkTargets.length > 0 && (
          <DriftSection title={`Invalid FK Targets (${drift.invalidFkTargets.length})`} titleClass="red" description="FK mappings to non-existent tables">
            {drift.invalidFkTargets.map(({ column, target }) => (
              <div key={column} className="drift-item">
                {column} &rarr; <span className="drift-col">{target}</span>
              </div>
            ))}
          </DriftSection>
        )}

        {cd && cd.missingDbConstraints.length > 0 && (
          <DriftSection title={`Missing DB Constraints (${cd.missingDbConstraints.length})`} titleClass="amber" description="In config but no actual DB foreign key constraint">
            {cd.missingDbConstraints.map(({ table, column, target }) => (
              <div key={`${table}.${column}`} className="drift-item clickable" onClick={() => goTo(table)}>
                <span className="drift-table">{table}</span>.<span className="drift-col">{column}</span> &rarr; {target}
              </div>
            ))}
          </DriftSection>
        )}

        {cd && cd.extraDbConstraints.length > 0 && (
          <DriftSection title={`Extra DB Constraints (${cd.extraDbConstraints.length})`} titleClass="blue" description="DB has FK constraint but no config mapping">
            {cd.extraDbConstraints.map(({ table, column, referencedTable }) => (
              <div key={`${table}.${column}`} className="drift-item clickable" onClick={() => goTo(table)}>
                <span className="drift-table">{table}</span>.<span className="drift-col">{column}</span> &rarr; {referencedTable}
              </div>
            ))}
          </DriftSection>
        )}

        {cd && cd.mismatchedFkTargets.length > 0 && (
          <DriftSection title={`Mismatched FK Targets (${cd.mismatchedFkTargets.length})`} titleClass="red" description="Config and DB disagree on FK target table">
            {cd.mismatchedFkTargets.map(({ table, column, configTarget, actualTarget }) => (
              <div key={`${table}.${column}`} className="drift-item clickable" onClick={() => goTo(table)}>
                <span className="drift-table">{table}</span>.<span className="drift-col">{column}</span>: config &rarr; {configTarget}, DB &rarr; {actualTarget}
              </div>
            ))}
          </DriftSection>
        )}
      </div>
    </>
  );
}
