"""refresh tokens: families, rotated_at, replaced_by_id

Revision ID: d0e1f2a3b4c5
Revises: c9d0e1f2a3b4
Create Date: 2026-09-25 19:45:00.000000

"""
from typing import Sequence, Union

import sqlalchemy as sa
from alembic import op

# revision identifiers, used by Alembic.
revision: str = "d0e1f2a3b4c5"
down_revision: Union[str, Sequence[str], None] = "c9d0e1f2a3b4"
branch_labels: Union[str, Sequence[str], None] = None
depends_on: Union[str, Sequence[str], None] = None


def upgrade() -> None:
    """Upgrade schema."""
    with op.batch_alter_table("refresh_tokens", schema=None) as batch_op:
        batch_op.add_column(sa.Column("family_id", sa.Uuid(), nullable=True))
        batch_op.add_column(sa.Column("rotated_at", sa.DateTime(timezone=True), nullable=True))
        batch_op.add_column(sa.Column("replaced_by_id", sa.Uuid(), nullable=True))
    # Existing tokens: each its own family, and none marked rotated. A revoked one
    # presented again is then a plain 401, never a theft verdict on old data.
    op.execute("UPDATE refresh_tokens SET family_id = id")
    with op.batch_alter_table("refresh_tokens", schema=None) as batch_op:
        batch_op.alter_column("family_id", existing_type=sa.Uuid(), nullable=False)
        batch_op.create_index("ix_refresh_tokens_family_id", ["family_id"], unique=False)


def downgrade() -> None:
    """Downgrade schema."""
    with op.batch_alter_table("refresh_tokens", schema=None) as batch_op:
        batch_op.drop_index("ix_refresh_tokens_family_id")
        batch_op.drop_column("replaced_by_id")
        batch_op.drop_column("rotated_at")
        batch_op.drop_column("family_id")
