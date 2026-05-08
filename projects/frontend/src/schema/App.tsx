import { useState } from 'react';
import { TabBar } from './components/TabBar';
import { SchemaView } from './components/SchemaView';
import { useTheme } from './hooks/useTheme';

export function App({ data, initialTabName }: { data: SchemaData; initialTabName?: string }) {
  const { tabs, showTabs } = data;
  const { palette, mode, setPalette, toggleMode } = useTheme();

  const initialIndex = initialTabName
    ? Math.max(0, tabs.findIndex(t => t.title.toLowerCase() === initialTabName.toLowerCase()))
    : 0;

  const [activeTab, setActiveTab] = useState(initialIndex);

  function selectTab(index: number) {
    setActiveTab(index);
    const name = tabs[index]?.title?.toLowerCase();
    if (name) window.history.replaceState({}, '', `/schema/${name}`);
  }

  return (
    <div className={`schema-page${showTabs ? ' has-tabs' : ''}`}>
      {showTabs && <TabBar tabs={tabs} activeIndex={activeTab} onSelect={selectTab} />}
      <SchemaView
        key={activeTab}
        data={tabs[activeTab]}
        palette={palette}
        mode={mode}
        onPaletteChange={setPalette}
        onToggleMode={toggleMode}
      />
    </div>
  );
}
