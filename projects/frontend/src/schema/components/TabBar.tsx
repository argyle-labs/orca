import { cx } from '../utils/utils';

export function TabBar({ tabs, activeIndex, onSelect }: { tabs: { title: string }[]; activeIndex: number; onSelect: (index: number) => void }) {
  return (
    <div id="tab-bar">
      {tabs.map((tab, i) => (
        <button key={i} className={cx('tab-item', i === activeIndex && 'active')} onClick={() => onSelect(i)}>
          {tab.title}
        </button>
      ))}
    </div>
  );
}
