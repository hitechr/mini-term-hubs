const assert = require('node:assert/strict');
const { test, beforeEach } = require('node:test');

// 该模块只用到 document/window 的事件收发与 body.classList，用极小替身即可直测。
// 事件分发对监听器列表取副本：onUp 会在分发过程中移除自己。
const bus = new Map();
let bodyClasses = new Set();

function listenerHost() {
  return {
    addEventListener: (type, fn) => {
      if (!bus.has(type)) bus.set(type, []);
      bus.get(type).push(fn);
    },
    removeEventListener: (type, fn) => {
      const list = bus.get(type);
      if (!list) return;
      const i = list.indexOf(fn);
      if (i >= 0) list.splice(i, 1);
    },
  };
}

function dispatch(type, event) {
  for (const fn of [...(bus.get(type) ?? [])]) fn(event);
}

global.document = {
  ...listenerHost(),
  body: {
    classList: {
      add: (c) => bodyClasses.add(c),
      remove: (c) => bodyClasses.delete(c),
      contains: (c) => bodyClasses.has(c),
    },
  },
};
global.window = listenerHost();

const {
  initProjectDrag,
  isProjectDragging,
} = require('../.tmp-tests/utils/projectDragState.js');

const row = () => ({ style: { opacity: '' } });
const proj = (id) => ({ type: 'project', projectId: id });

beforeEach(() => {
  bus.clear();
  bodyClasses = new Set();
});

test('正常拖放后源行恢复不透明', () => {
  const el = row();
  initProjectDrag(proj('a'), el, 0, 0);
  dispatch('mousemove', { clientX: 50, clientY: 0 });
  assert.equal(el.style.opacity, '0.4');

  dispatch('mouseup', {});
  assert.equal(el.style.opacity, '');
  assert.equal(bodyClasses.has('project-dragging'), false);
  assert.equal(isProjectDragging(), false);
});

test('未越过 5px 阈值的点击不改透明度', () => {
  const el = row();
  initProjectDrag(proj('a'), el, 0, 0);
  dispatch('mousemove', { clientX: 2, clientY: 1 });
  dispatch('mouseup', {});
  assert.equal(el.style.opacity, '');
});

// 回归：mouseup 丢失（拖出 WebView / 中途失焦）后用户又按下另一行，
// 后一次 initProjectDrag 重置了模块级 _dragging，前一行的收尾若以它为条件
// 就会被跳过，源行永久停在 40% 透明度（重启才恢复）。
test('前一次拖拽的 mouseup 迟到时，该行透明度仍被复位', () => {
  const elA = row();
  initProjectDrag(proj('a'), elA, 0, 0);
  dispatch('mousemove', { clientX: 50, clientY: 0 });
  assert.equal(elA.style.opacity, '0.4');

  const elB = row();
  initProjectDrag(proj('b'), elB, 0, 0);

  dispatch('mouseup', {});
  assert.equal(elA.style.opacity, '', 'A 行残留半透明');
});

test('前一次拖拽的 mouseup 迟到时，body 的 grabbing 光标类仍被摘掉', () => {
  const elA = row();
  initProjectDrag(proj('a'), elA, 0, 0);
  dispatch('mousemove', { clientX: 50, clientY: 0 });
  assert.equal(bodyClasses.has('project-dragging'), true);

  initProjectDrag(proj('b'), row(), 0, 0);
  dispatch('mouseup', {});
  assert.equal(bodyClasses.has('project-dragging'), false, 'body 残留 project-dragging');
});
