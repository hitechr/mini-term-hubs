const assert = require('node:assert/strict');
const { test } = require('node:test');

const {
  restoreSavedProjectLayout,
  restoreSavedSplitNode,
} = require('../.tmp-tests/utils/layoutRestore.js');

const config = {
  availableShells: [
    { name: 'nushell', command: 'nu' },
    { name: 'cmd', command: 'cmd' },
  ],
  defaultShell: 'cmd',
};

function idFactory() {
  let n = 0;
  return () => `pane-${++n}`;
}

// 旧配置形态：项目级 tab 层已删除，恢复时多出来的树要平铺进保留那棵的最左 leaf
const legacySavedLayout = {
  activeTabIndex: 1,
  tabs: [
    {
      customTitle: 'first',
      splitLayout: { type: 'leaf', panes: [{ shellName: 'nushell' }] },
    },
    {
      splitLayout: {
        type: 'split',
        direction: 'horizontal',
        sizes: [40, 60],
        children: [
          { type: 'leaf', panes: [{ shellName: 'missing-shell' }] },
          { type: 'leaf', panes: [{ shellName: 'nushell' }, { shellName: 'cmd' }] },
        ],
      },
    },
  ],
};

test('恢复保留 activeTabIndex 指向的那棵树', () => {
  const restored = restoreSavedProjectLayout('project-1', legacySavedLayout, config, idFactory());
  assert.equal(restored.id, 'project-1');
  assert.equal(restored.status, 'idle');
  assert.equal(restored.layout.type, 'split');
  assert.deepEqual(restored.layout.sizes, [40, 60]);
});

test('被丢弃的旧 tab 里的 pane 平铺进最左 leaf,一个终端不少', () => {
  const restored = restoreSavedProjectLayout('project-1', legacySavedLayout, config, idFactory());
  const [left, right] = restored.layout.children;
  // 左 leaf：自身的 missing-shell（回落 defaultShell）+ 从 tab0 平铺过来的 nushell
  assert.deepEqual(left.panes.map((p) => p.shellName), ['cmd', 'nushell']);
  assert.deepEqual(right.panes.map((p) => p.shellName), ['nushell', 'cmd']);
});

test('认不出的 shell 回落到 defaultShell', () => {
  const saved = { type: 'leaf', panes: [{ shellName: 'missing-shell' }] };
  const node = restoreSavedSplitNode(saved, config, idFactory());
  assert.equal(node.panes[0].shellName, 'cmd');
});

test('恢复出的 pane 不带 ptyId,状态为 idle', () => {
  const restored = restoreSavedProjectLayout('project-1', legacySavedLayout, config, idFactory());
  const pane = restored.layout.children[0].panes[0];
  assert.equal(Object.hasOwn(pane, 'ptyId'), false);
  assert.equal(pane.status, 'idle');
});

test('带 aiSession 的 pane 恢复后置位 resumePending', () => {
  const saved = {
    type: 'leaf',
    panes: [{ shellName: 'cmd', aiSession: { agent: 'claude', sessionId: 'abc-123' } }],
  };
  const node = restoreSavedSplitNode(saved, config, idFactory());
  assert.equal(node.panes[0].aiSession.sessionId, 'abc-123');
  assert.equal(node.panes[0].resumePending, true);
});

// 回归：用户给 pane 起的名字要活过重启（此前 customTitle 根本不进持久化格式，
// 重启后 tab 上的名字回落成 shell 名）
test('恢复保留 pane 的自定义名', () => {
  const saved = {
    type: 'leaf',
    panes: [{ shellName: 'cmd', customTitle: '修复 otel 传递' }, { shellName: 'nushell' }],
  };
  const node = restoreSavedSplitNode(saved, config, idFactory());
  assert.equal(node.panes[0].customTitle, '修复 otel 传递');
  assert.equal(node.panes[1].customTitle, undefined, '没起过名的 pane 不该凭空多出名字');
});
