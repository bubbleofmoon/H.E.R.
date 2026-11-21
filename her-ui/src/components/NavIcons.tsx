import React from 'react';
import {
  Activity,
  Archive,
  BookOpen,
  Bot,
  CalendarClock,
  Cpu,
  FileSearch,
  Gauge,
  MessageSquare,
  ServerCog,
  Shield,
  SlidersHorizontal,
} from 'lucide-react';
import { NavItem } from '../catalog/navigation';

export const NavIcon: React.FC<{ id: NavItem['id']; className?: string }> = ({
  id,
  className = 'h-4 w-4',
}) => {
  switch (id) {
    case 'overview':
      return <Gauge className={className} />;
    case 'conversations':
      return <MessageSquare className={className} />;
    case 'persona':
      return <Bot className={className} />;
    case 'memory':
      return <BookOpen className={className} />;
    case 'behavior':
      return <Activity className={className} />;
    case 'models':
      return <Cpu className={className} />;
    case 'settings':
      return <SlidersHorizontal className={className} />;
    case 'schedule':
      return <CalendarClock className={className} />;
    case 'system':
      return <ServerCog className={className} />;
    case 'security':
      return <Shield className={className} />;
    case 'audit':
      return <FileSearch className={className} />;
    case 'legacy':
      return <Archive className={className} />;
  }
};
