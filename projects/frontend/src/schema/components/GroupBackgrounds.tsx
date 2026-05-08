export function GroupBackgrounds({ groups }: { groups: Group[] }) {
  return (
    <>
      {groups.flatMap((group) => {
        const isGrouped = group.subs.length > 1 || group.subs[0].domain?.group;

        return [
          isGrouped && (
            <div
              key={`g-${group.key}`}
              className="group-bg"
              style={{
                left: group.x,
                top: group.y,
                width: group.w,
                height: group.h,
                borderColor: group.color + '25',
                background: group.color + '06'
              }}
            >
              <div className="group-label" style={{ color: group.color }}>
                {group.label}
              </div>
            </div>
          ),
          ...group.subs.map((sub) => (
            <div
              key={`d-${sub.domain.key}`}
              className="domain-bg"
              style={{
                left: sub.x,
                top: sub.y,
                width: sub.w,
                height: sub.h,
                borderColor: sub.domain.color + '30',
                background: sub.domain.color + '08'
              }}
            >
              <div className="domain-label" style={{ color: sub.domain.color }}>
                {sub.domain.label}
              </div>
            </div>
          ))
        ];
      })}
    </>
  );
}
