---@meta

---@class MluaHttpRequest

---@class MluaHttpResponse
---@field ok fun(self: MluaHttpResponse): boolean
---@field url fun(self: MluaHttpResponse): string
---@field headers fun(self: MluaHttpResponse): table<string, string>
---@field header_pairs fun(self: MluaHttpResponse): string[][]
---@field body_state fun(self: MluaHttpResponse): string
---@field bytes fun(self: MluaHttpResponse, options?: {max_bytes?: integer, timeout_ms?: integer}): string
---@field status fun(self: MluaHttpResponse): integer
---@field header_values fun(self: MluaHttpResponse, name: string): string[]
---@field close fun(self: MluaHttpResponse)
---@field json fun(self: MluaHttpResponse, options?: {codec?: string, max_bytes?: integer, timeout_ms?: integer}): any
---@field text fun(self: MluaHttpResponse, options?: {max_bytes?: integer, timeout_ms?: integer}): string

---@class MluaHttpBuilder
---@field json_codec fun(self: MluaHttpBuilder, codec: string): MluaHttpBuilder
---@field query_pairs fun(self: MluaHttpBuilder, pairs: string[][]): MluaHttpBuilder
---@field header_pairs fun(self: MluaHttpBuilder, pairs: string[][]): MluaHttpBuilder
---@field json fun(self: MluaHttpBuilder, value: any): MluaHttpBuilder
---@field stream fun(self: MluaHttpBuilder, options?: {headers_timeout_ms?: integer}): MluaHttpBuilder
---@field build fun(self: MluaHttpBuilder): MluaHttpRequest

---@class MluaHttpClient
---@field request fun(self: MluaHttpClient, method: string, url: string): MluaHttpBuilder
---@field send fun(self: MluaHttpClient, request: MluaHttpRequest): MluaHttpResponse
