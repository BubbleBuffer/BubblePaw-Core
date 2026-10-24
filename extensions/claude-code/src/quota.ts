// Only validated meter fields cross the extension boundary. The SDK sends
// utilization as a fraction; the separate OAuth usage API reports percentages.
export interface RateLimitInfo {
	status: "allowed" | "allowed_warning" | "rejected";
	utilization?: number;
	rateLimitType?: string;
	resetsAt?: number;
	unifiedWindows?: Record<string, { utilization: number; resetsAt?: number }>;
	overageStatus?: "allowed" | "allowed_warning" | "rejected";
	overageResetsAt?: number;
	overageDisabledReason?: string;
	isUsingOverage?: boolean;
	overageInUse?: boolean;
	surpassedThreshold?: number;
	limitScope?: string;
	errorCode?: string;
	canUserPurchaseCredits?: boolean;
	hasChargeableSavedPaymentMethod?: boolean;
}
const label = (value: unknown): value is string => typeof value === "string" && /^[a-zA-Z0-9_.:-]{1,100}$/.test(value);
const epoch = (value: unknown): value is number => typeof value === "number" && Number.isFinite(value) && value >= 0 && value <= 253402300799;
const fraction = (value: unknown): value is number => typeof value === "number" && Number.isFinite(value) && value >= 0 && value <= 1;
const status = (value: unknown): value is RateLimitInfo["status"] => ["allowed", "allowed_warning", "rejected"].includes(value as string);

export function rateLimitInfo(value: any): RateLimitInfo | undefined {
	if (!value || !status(value.status)) return undefined;
	const info: RateLimitInfo = { status: value.status };
	if (fraction(value.utilization)) info.utilization = value.utilization;
	if (label(value.rateLimitType)) info.rateLimitType = value.rateLimitType;
	if (epoch(value.resetsAt)) info.resetsAt = value.resetsAt;
	if (value.unifiedWindows && typeof value.unifiedWindows === "object") {
		const windows: NonNullable<RateLimitInfo["unifiedWindows"]> = {};
		for (const [key, meter] of Object.entries(value.unifiedWindows) as [string, any][]) {
			if (label(key) && fraction(meter?.utilization)) windows[key] = { utilization: meter.utilization, ...(epoch(meter.resetsAt) ? { resetsAt: meter.resetsAt } : {}) };
		}
		if (Object.keys(windows).length) info.unifiedWindows = windows;
	}
	if (status(value.overageStatus)) info.overageStatus = value.overageStatus;
	if (epoch(value.overageResetsAt)) info.overageResetsAt = value.overageResetsAt;
	for (const key of ["overageDisabledReason", "limitScope", "errorCode"] as const) if (label(value[key])) info[key] = value[key];
	for (const key of ["isUsingOverage", "overageInUse", "canUserPurchaseCredits", "hasChargeableSavedPaymentMethod"] as const) if (typeof value[key] === "boolean") info[key] = value[key];
	if (fraction(value.surpassedThreshold)) info.surpassedThreshold = value.surpassedThreshold;
	return info;
}

export function terminalRateLimit(info: RateLimitInfo): boolean {
	return info.status === "rejected" && (info.errorCode === "credits_required" || info.overageDisabledReason === "out_of_credits");
}
