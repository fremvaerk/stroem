import { useMemo } from "react";
import { Link, useLocation } from "react-router";
import {
  BookOpen,
  FolderOpen,
  LayoutDashboard,
  ListChecks,
  Play,
  LogOut,
  Server,
  Settings,
  Users,
  Zap,
} from "lucide-react";
import { useAuth } from "@/context/auth-context";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarGroupContent,
  SidebarHeader,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarSeparator,
} from "@/components/ui/sidebar";

const navItems = [
  { title: "Dashboard", href: "/", icon: LayoutDashboard },
  { title: "Workspaces", href: "/workspaces", icon: FolderOpen },
  { title: "Tasks", href: "/tasks", icon: ListChecks },
  { title: "Jobs", href: "/jobs", icon: Play },
  { title: "Workers", href: "/workers", icon: Server },
];

function isActive(pathname: string, href: string): boolean {
  if (href === "/") return pathname === "/";
  return pathname.startsWith(href);
}

export function AppSidebar() {
  const { pathname } = useLocation();
  const { user, authRequired, serverVersion, logout, isAdmin } = useAuth();

  const adminItems = useMemo(() => {
    if (!authRequired || !isAdmin) return [];
    return [{ title: "Users", href: "/users", icon: Users }];
  }, [authRequired, isAdmin]);

  return (
    <Sidebar>
      <SidebarHeader>
        <SidebarMenu>
          <SidebarMenuItem>
            <SidebarMenuButton size="lg" asChild>
              <Link to="/">
                <div className="flex aspect-square size-8 items-center justify-center rounded-lg bg-primary text-primary-foreground">
                  <Zap className="size-4" />
                </div>
                <div className="grid flex-1 text-left text-sm leading-tight">
                  <span className="truncate font-semibold tracking-tight">
                    Strøm
                  </span>
                  <span className="truncate text-xs text-muted-foreground">
                    {serverVersion ? `v${serverVersion}` : "Workflow Platform"}
                  </span>
                </div>
              </Link>
            </SidebarMenuButton>
          </SidebarMenuItem>
        </SidebarMenu>
      </SidebarHeader>

      <SidebarContent>
        <SidebarGroup>
          <SidebarGroupContent>
            <SidebarMenu>
              {navItems.map((item) => (
                <SidebarMenuItem key={item.href}>
                  <SidebarMenuButton
                    asChild
                    isActive={isActive(pathname, item.href)}
                    tooltip={item.title}
                  >
                    <Link to={item.href}>
                      <item.icon />
                      <span>{item.title}</span>
                    </Link>
                  </SidebarMenuButton>
                </SidebarMenuItem>
              ))}
            </SidebarMenu>
          </SidebarGroupContent>
        </SidebarGroup>
      </SidebarContent>

      <SidebarFooter>
        <SidebarMenu>
          {adminItems.map((item) => (
            <SidebarMenuItem key={item.href}>
              <SidebarMenuButton
                asChild
                isActive={isActive(pathname, item.href)}
                tooltip={item.title}
              >
                <Link to={item.href}>
                  <item.icon />
                  <span>{item.title}</span>
                </Link>
              </SidebarMenuButton>
            </SidebarMenuItem>
          ))}
          <SidebarMenuItem>
            <SidebarMenuButton asChild tooltip="Documentation">
              <a
                href="https://fremvaerk.github.io/stroem/"
                target="_blank"
                rel="noopener noreferrer"
              >
                <BookOpen />
                <span>Documentation</span>
              </a>
            </SidebarMenuButton>
          </SidebarMenuItem>
        </SidebarMenu>
        {authRequired && user && (
          <>
            <SidebarSeparator />
            <div className="flex items-center gap-2 px-2 py-1">
              <Avatar className="h-8 w-8">
                <AvatarFallback className="text-xs">
                  {user.name
                    ? user.name
                        .split(" ")
                        .slice(0, 2)
                        .map((w) => w[0])
                        .join("")
                        .toUpperCase()
                    : user.email[0].toUpperCase()}
                </AvatarFallback>
              </Avatar>
              <div className="grid flex-1 text-left leading-tight">
                <p className="truncate text-sm font-medium">{user.name}</p>
                <p className="truncate text-xs text-muted-foreground">
                  {user.email}
                </p>
              </div>
            </div>
            <SidebarMenu>
              <SidebarMenuItem>
                <SidebarMenuButton
                  asChild
                  isActive={isActive(pathname, "/settings")}
                  tooltip="Settings"
                >
                  <Link to="/settings">
                    <Settings />
                    <span>Settings</span>
                  </Link>
                </SidebarMenuButton>
              </SidebarMenuItem>
              <SidebarMenuItem>
                <SidebarMenuButton onClick={logout} tooltip="Sign out">
                  <LogOut />
                  <span>Sign out</span>
                </SidebarMenuButton>
              </SidebarMenuItem>
            </SidebarMenu>
          </>
        )}
      </SidebarFooter>
    </Sidebar>
  );
}
