import { z } from 'zod'

export const ForkWorkspaceSchema = {
  source: z.string().min(1).describe('id or name of the workspace to clone'),
  name: z.string().min(1).optional().describe('name for the new forked workspace'),
}

export const MountSchema = {
  workspace: z.string().min(1),
  path: z.string().min(1).describe('absolute or relative local path to mount onto'),
}

export const ListWorkspacesSchema = {
  filter: z.string().optional().describe('optional substring to filter by name or status'),
}

export const PushSchema = {
  workspace: z.string().min(1),
  message: z.string().optional().describe('optional commit message'),
}

export const GrantAccessSchema = {
  workspace: z.string().min(1),
  grantee: z.string().min(1).describe('user id or email to grant'),
  role: z.enum(['read', 'write', 'admin']),
}

export const DestroySchema = {
  workspace: z.string().min(1),
}
