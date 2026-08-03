import { Redirect } from '@/components/Redirect'

export function generateStaticParams() {
  return [{ chatId: 'placeholder' }]
}

export default function ChatThreadRedirectPage() {
  return <Redirect to="/brain" />
}
