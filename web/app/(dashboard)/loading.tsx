import { SkeletonPageHeader, SkeletonStatCards, SkeletonCard, SkeletonBox } from '@/components/loading/skeletons';

export default function DashboardLoading() {
  return (
    <div className="space-y-6">
      <SkeletonPageHeader />
      <SkeletonStatCards count={4} />
      <div className="grid gap-6 lg:grid-cols-2">
        <SkeletonCard />
        <SkeletonCard />
      </div>
      <SkeletonBox className="h-48" />
    </div>
  );
}
