export type NavGroupId = 'run' | 'her' | 'config' | 'system' | 'security' | 'legacy';

export type PageId =
  | 'overview'
  | 'conversations'
  | 'persona'
  | 'memory'
  | 'behavior'
  | 'models'
  | 'settings'
  | 'schedule'
  | 'system'
  | 'security'
  | 'audit'
  | 'legacy'
  | 'login'
  | 'setup';

export type InspectorTarget =
  | { kind: 'conversation'; id: string }
  | { kind: 'persona'; id: string }
  | { kind: 'setting'; key: string }
  | { kind: 'audit'; id: string }
  | null;

export interface NavTab {
  id: string;
  label: string;
  pinyin: string;
}

export interface NavItem {
  id: Exclude<PageId, 'login' | 'setup'>;
  label: string;
  shortLabel: string;
  path: string;
  pinyin: string;
  groupId: NavGroupId;
  groupLabel: string;
  description: string;
  sliceTicket: string;
  tabs?: NavTab[];
}

export const NAV_GROUPS: { id: Exclude<NavGroupId, 'legacy'>; label: string }[] = [
  { id: 'run', label: '运行' },
  { id: 'her', label: 'HER' },
  { id: 'config', label: '配置' },
  { id: 'system', label: '系统' },
  { id: 'security', label: '安全' },
];

export const NAV_ITEMS: NavItem[] = [
  {
    id: 'overview',
    label: '总览',
    shortLabel: '总览',
    path: '/',
    pinyin: 'zl',
    groupId: 'run',
    groupLabel: '运行',
    sliceTicket: '#356',
    description: '查看实时运行状态、待处理审批与提问、快速模式切换及近期活动。',
  },
  {
    id: 'conversations',
    label: '会话',
    shortLabel: '会话',
    path: '/conversations',
    pinyin: 'hh',
    groupId: 'run',
    groupLabel: '运行',
    sliceTicket: '#356',
    description: '管理允许会话的接入状态、唤醒进度、单会话设置、人设与恢复。',
  },
  {
    id: 'persona',
    label: '人设与提示词',
    shortLabel: '人设',
    path: '/persona',
    pinyin: 'rsytsc',
    groupId: 'her',
    groupLabel: 'HER',
    sliceTicket: '#357',
    description: '维护版本化人设库、唤醒词与模式/仿真提示词。',
    tabs: [
      { id: 'personas', label: '人设库', pinyin: 'rsk' },
      { id: 'prompts', label: '仿真提示词', pinyin: 'fztsc' },
    ],
  },
  {
    id: 'memory',
    label: '记忆与知识',
    shortLabel: '记忆',
    path: '/memory',
    pinyin: 'jyyzs',
    groupId: 'her',
    groupLabel: 'HER',
    sliceTicket: '#357',
    description: '检索与维护记忆条目、黑话词库、表情包收藏与语音库。',
    tabs: [
      { id: 'library', label: '记忆库', pinyin: 'jyk' },
      { id: 'slang', label: '黑话词库', pinyin: 'hhck' },
      { id: 'media', label: '表情包与语音', pinyin: 'bqbyyy' },
    ],
  },
  {
    id: 'behavior',
    label: '参与模式与参数',
    shortLabel: '参与模式',
    path: '/behavior',
    pinyin: 'cymsycs',
    groupId: 'config',
    groupLabel: '配置',
    sliceTicket: '#358',
    description: '配置参与模式、二代仿真实时设置项与诊断日志。',
    tabs: [
      { id: 'params', label: '模式与参数', pinyin: 'msycs' },
      { id: 'diagnostics', label: '诊断日志', pinyin: 'zdrz' },
    ],
  },
  {
    id: 'models',
    label: '模型与用量',
    shortLabel: '模型用量',
    path: '/models',
    pinyin: 'mxyyl',
    groupId: 'config',
    groupLabel: '配置',
    sliceTicket: '#358',
    description: '管理全局模型路由链、查看路由健康及用量记录与花费趋势。',
    tabs: [
      { id: 'routes', label: '模型路由', pinyin: 'mxly' },
      { id: 'spend', label: '用量与花费', pinyin: 'ylyhf' },
    ],
  },
  {
    id: 'settings',
    label: '全部设置',
    shortLabel: '设置',
    path: '/settings',
    pinyin: 'qbsz',
    groupId: 'config',
    groupLabel: '配置',
    sliceTicket: '#358',
    description: '检索与编辑全量全局实时设置，并查看或撤销变更历史。',
    tabs: [
      { id: 'registry', label: '设置索引', pinyin: 'szsy' },
      { id: 'history', label: '变更历史', pinyin: 'bgls' },
    ],
  },
  {
    id: 'schedule',
    label: '日程与任务',
    shortLabel: '日程',
    path: '/schedule',
    pinyin: 'rcyrw',
    groupId: 'system',
    groupLabel: '系统',
    sliceTicket: '#359',
    description: '统一管理定时日程、提醒事项、任务执行历史与实时日志。',
    tabs: [
      { id: 'upcoming', label: '即将执行与提醒', pinyin: 'jjzxytx' },
      { id: 'runs', label: '任务运行历史', pinyin: 'rwyxls' },
    ],
  },
  {
    id: 'system',
    label: '运维与备份',
    shortLabel: '运维备份',
    path: '/system',
    pinyin: 'ywybf',
    groupId: 'system',
    groupLabel: '系统',
    sliceTicket: '#359',
    description: '管理运行控制、备份与还原及 HER 版本更新与依赖。',
    tabs: [
      { id: 'runtime', label: '运行控制', pinyin: 'yxkz' },
      { id: 'backups', label: '备份与还原', pinyin: 'bfyhy' },
      { id: 'updates', label: '更新与依赖', pinyin: 'gxyyl' },
    ],
  },
  {
    id: 'security',
    label: '安全与访问',
    shortLabel: '安全',
    path: '/security',
    pinyin: 'aqyfw',
    groupId: 'security',
    groupLabel: '安全',
    sliceTicket: '#360',
    description: '管理管理员密码、浏览器会话、黑白名单、记忆权限与控制台令牌。',
  },
  {
    id: 'audit',
    label: '审计与恢复',
    shortLabel: '审计恢复',
    path: '/audit',
    pinyin: 'sjyhf',
    groupId: 'security',
    groupLabel: '安全',
    sliceTicket: '#360',
    description: '查看并撤销全量审计记录，处理卡住轮次与入站缺口。',
    tabs: [
      { id: 'audit-log', label: '审计历史', pinyin: 'sjls' },
      { id: 'recovery', label: '恢复队列', pinyin: 'hfdl' },
    ],
  },
  {
    id: 'legacy',
    label: '旧版',
    shortLabel: '旧版兼容',
    path: '/legacy',
    pinyin: 'jb',
    groupId: 'legacy',
    groupLabel: '旧版',
    sliceTicket: '#361',
    description: '一代仿真与 DSH 专用工具（将在 #78 完成迁移后整体移除）。',
  },
];

export const LEGACY_REDIRECTS: Record<string, string> = {
  '/personas': '/persona',
  '/tasks': '/schedule',
  '/settings/conversations': '/conversations',
  '/settings/models': '/models',
};

export function pathForPage(page: PageId): string {
  if (page === 'login') return '/login';
  if (page === 'setup') return '/setup';
  return NAV_ITEMS.find((item) => item.id === page)?.path ?? '/';
}

export function pageForPath(rawPathname: string): PageId {
  const normalized = LEGACY_REDIRECTS[rawPathname] ?? rawPathname;
  if (normalized === '/login') return 'login';
  if (normalized === '/setup') return 'setup';
  const match = NAV_ITEMS.find((item) => item.path === normalized);
  return match?.id ?? 'overview';
}

export function sanitizeNextPath(rawNext: string | null): string | null {
  if (!rawNext || !rawNext.startsWith('/') || rawNext.startsWith('//')) {
    return null;
  }
  if (rawNext.startsWith('/login') || rawNext.startsWith('/setup')) {
    return null;
  }
  return rawNext;
}
