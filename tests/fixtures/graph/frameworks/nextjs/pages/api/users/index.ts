import type { NextApiRequest, NextApiResponse } from 'next';

export default async function usersHandler(req: NextApiRequest, res: NextApiResponse) {
  res.status(200).json([]);
}
